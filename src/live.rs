//! Read-only, best-effort live telemetry (saved players are not an online list).
//!
//! Identities are an optional JSON object: UUID -> {"name":"…","head":"heads/….png"}.
//! Without identities, names are UUIDs. Heads must be local relative URLs. Missing
//! stats omit a player; the production threshold is strictly > 1,728,000 ticks.
//! Activity means changed saved LastUpdate counters, not authoritative presence.
use crate::{
    config::{Config, Dimension},
    output::atomic_write,
    world::World,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    io::Read,
    path::Path,
    time::UNIX_EPOCH,
};

const MAX_NBT: usize = 16 * 1024 * 1024;
const MIN_PLAY_TICKS: u64 = 1_728_000;
const MAX_OVERLAY: usize = 1_000_000;
type Position = (i32, i32);

#[derive(Deserialize)]
struct PlayerNbt {
    #[serde(rename = "Dimension")]
    dimension: i32,
    #[serde(rename = "Pos")]
    pos: Vec<f64>,
}
#[derive(Default, Deserialize)]
struct Identity {
    name: String,
    head: Option<String>,
}
#[derive(Serialize)]
struct Player {
    uuid: String,
    name: String,
    world: String,
    dimension: i32,
    x: f64,
    y: f64,
    z: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
}
#[derive(Serialize)]
struct Players {
    updated_at: u64,
    players: Vec<Player>,
}
#[derive(Serialize)]
struct ActivityChunk {
    world: String,
    x: i32,
    z: i32,
    r#type: u8,
}
#[derive(Serialize)]
struct Activity {
    updated_at: u64,
    chunks: Vec<ActivityChunk>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    size: u64,
    seconds: u64,
    nanos: u32,
}
#[derive(Clone, Serialize, Deserialize)]
struct RegionState {
    stamp: Stamp,
    counters: BTreeMap<String, i64>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct WorldState {
    directory: String,
    regions: BTreeMap<String, RegionState>,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    worlds: BTreeMap<String, WorldState>,
}

/// Publish independent, atomic player/activity snapshots and a durable checkpoint.
/// A separate kernel lock allows this to run concurrently with rendering.
pub fn update(config: &Config, players: &Path, identities: Option<&Path>) -> Result<()> {
    let state_dir = config.output.join(".state");
    fs::create_dir_all(&state_dir)?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(state_dir.join("live.lock"))?;
    lock.try_lock()
        .context("another live updater is running for this output")?;
    let root = players
        .parent()
        .context("playerdata must have a world root parent")?
        .canonicalize()?;
    let identities: BTreeMap<String, Identity> = match identities {
        Some(path) => match read_json(path) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("identities unavailable; using UUIDs: {error:#}");
                BTreeMap::new()
            }
        },
        None => BTreeMap::new(),
    };
    let mut selected = BTreeMap::new();
    for (id, world) in &config.worlds {
        if world.path.canonicalize().ok().as_ref() == Some(&root) {
            let dimension = match world.dimension {
                Dimension::Overworld => 0,
                Dimension::Nether => -1,
                Dimension::End => 1,
            };
            // Aliases are deterministic: first configured ID owns player markers.
            selected.entry(dimension).or_insert(id.clone());
        }
    }
    ensure!(
        !selected.is_empty(),
        "no configured world owns {}",
        players.display()
    );
    let mut markers = Vec::new();
    let mut player_centers = BTreeMap::<String, Vec<Position>>::new();
    for entry in fs::read_dir(players).with_context(|| format!("reading {}", players.display()))? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!("player entry: {error}");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_none_or(|s| s != "dat") {
            continue;
        }
        let Some(uuid) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| valid_uuid(s))
        else {
            continue;
        };
        let result = (|| -> Result<Option<Player>> {
            let before = stamp(&path)?;
            let data = parse_player(fs::File::open(&path)?)?;
            ensure!(
                stamp(&path)? == before,
                "player changed during read; retry next update"
            );
            let Some(world) = selected.get(&data.dimension) else {
                return Ok(None);
            };
            // Activity is based on recently saved players, including newcomers;
            // historical markers and their playtime filter are independent.
            if crate::output::unix_time().saturating_sub(before.seconds) <= 90 {
                player_centers
                    .entry(world.clone())
                    .or_default()
                    .push((chunk_coordinate(data.pos[0]), chunk_coordinate(data.pos[2])));
            }
            let stats: BTreeMap<String, serde_json::Value> =
                read_json(&root.join("stats").join(format!("{uuid}.json")))?;
            if stats
                .get("stat.playOneMinute")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                <= MIN_PLAY_TICKS
            {
                return Ok(None);
            }
            let identity = identities.get(uuid);
            let name = identity
                .filter(|i| !i.name.is_empty())
                .map_or_else(|| uuid.to_owned(), |i| i.name.clone());
            let head = identity
                .and_then(|i| i.head.as_ref())
                .filter(|s| safe_head(s))
                .cloned();
            Ok(Some(Player {
                uuid: uuid.to_owned(),
                name,
                world: world.clone(),
                dimension: data.dimension,
                x: data.pos[0],
                y: data.pos[1],
                z: data.pos[2],
                head,
            }))
        })();
        match result {
            Ok(Some(player)) => markers.push(player),
            Ok(None) => {}
            Err(error) => tracing::warn!(player = uuid, "skipping player: {error:#}"),
        }
    }
    markers.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    let state_path = state_dir.join("live-index.json");
    // Unlike individual input documents, the checkpoint grows with world size.
    let old: State = match fs::read(&state_path)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(anyhow::Error::from))
    {
        Ok(state) => state,
        Err(error) => {
            if state_path.exists() {
                tracing::warn!("resetting unreadable live checkpoint: {error:#}");
            }
            State::default()
        }
    };
    let mut state = State::default();
    let mut chunks = Vec::new();
    let cache = config
        .cache_mib
        .saturating_mul(1024 * 1024)
        .min(32 * 1024 * 1024)
        / config.worlds.len().max(1);
    for (id, world_config) in &config.worlds {
        let directory = world_config.dimension.directory(&world_config.path);
        let directory_key = directory
            .canonicalize()
            .unwrap_or_else(|_| directory.clone())
            .to_string_lossy()
            .into_owned();
        let previous = old.worlds.get(id).filter(|s| s.directory == directory_key);
        let (next, changed) = match scan_activity(&directory, &directory_key, previous, cache) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(world = id, "skipping activity scan: {error:#}");
                (
                    previous.cloned().unwrap_or_else(|| WorldState {
                        directory: directory_key,
                        regions: BTreeMap::new(),
                    }),
                    BTreeSet::new(),
                )
            }
        };
        state.worlds.insert(id.clone(), next);
        let centers = player_centers
            .get(id)
            .into_iter()
            .flat_map(|v| v.iter().copied());
        let remaining = MAX_OVERLAY.saturating_sub(chunks.len());
        for (x, z, kind) in classify(&changed, centers, remaining) {
            chunks.push(ActivityChunk {
                world: id.clone(),
                x,
                z,
                r#type: kind,
            });
        }
    }
    let updated_at = crate::output::unix_time();
    atomic_write(
        &config.output.join("live/players.json"),
        &serde_json::to_vec(&Players {
            updated_at,
            players: markers,
        })?,
    )?;
    atomic_write(
        &config.output.join("live/chunks.json"),
        &serde_json::to_vec(&Activity { updated_at, chunks })?,
    )?;
    // Only advance counters after successfully publishing telemetry.
    atomic_write(&state_path, &serde_json::to_vec(&state)?)?;
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_NBT as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_NBT, "JSON exceeds 16 MiB");
    Ok(serde_json::from_slice(&bytes)?)
}
fn valid_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}
fn safe_head(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('/')
        && !s.contains([':', '\\', '%', '?', '#'])
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_/ .".contains(&b))
        && s.split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
        && !s.contains(' ')
}
fn chunk_coordinate(value: f64) -> i32 {
    (value / 16.0).floor() as i32
}
fn parse_player(reader: impl Read) -> Result<PlayerNbt> {
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(reader)
        .take(MAX_NBT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_NBT, "player NBT exceeds 16 MiB");
    validate_nbt(&bytes)?;
    let player: PlayerNbt =
        fastnbt::from_bytes_with_opts(&bytes, fastnbt::DeOpts::new().max_seq_len(MAX_NBT))?;
    ensure!(
        player.pos.len() == 3
            && player
                .pos
                .iter()
                .all(|p| p.is_finite() && p.abs() <= 32_000_000.0),
        "invalid player position"
    );
    Ok(player)
}
fn stamp(path: &Path) -> Result<Stamp> {
    let metadata = fs::metadata(path)?;
    let modified = metadata.modified()?.duration_since(UNIX_EPOCH)?;
    Ok(Stamp {
        size: metadata.len(),
        seconds: modified.as_secs(),
        nanos: modified.subsec_nanos(),
    })
}
fn region_position(name: &str) -> Option<Position> {
    let parts: Vec<_> = name.split('.').collect();
    if parts.len() != 4 || parts[0] != "r" || parts[3] != "mca" {
        return None;
    }
    Some((parts[1].parse().ok()?, parts[2].parse().ok()?))
}
fn scan_activity(
    directory: &Path,
    directory_key: &str,
    previous: Option<&WorldState>,
    cache: usize,
) -> Result<(WorldState, BTreeSet<Position>)> {
    let mut current = BTreeMap::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if region_position(&name).is_some() {
            current.insert(name, stamp(&entry.path())?);
        }
    }
    let mut next = WorldState {
        directory: directory_key.to_owned(),
        regions: BTreeMap::new(),
    };
    let dirty: BTreeSet<_> = current
        .iter()
        .filter(|(name, stamp)| {
            previous
                .and_then(|p| p.regions.get(*name))
                .is_none_or(|r| &r.stamp != *stamp)
        })
        .map(|(name, _)| name.clone())
        .collect();
    for (name, region) in previous.into_iter().flat_map(|p| &p.regions) {
        if current.contains_key(name) {
            next.regions.insert(name.clone(), region.clone());
        }
    }
    let mut changed = BTreeSet::new();
    if dirty.is_empty() {
        return Ok((next, changed));
    }
    // World opens headers only; decompress exclusively chunks in changed regions.
    let world = World::open(directory, cache)?;
    let mut positions: BTreeMap<Position, Vec<Position>> = BTreeMap::new();
    for (x, z) in world.chunk_positions() {
        positions
            .entry((x.div_euclid(32), z.div_euclid(32)))
            .or_default()
            .push((x, z));
    }
    for name in dirty {
        let region = region_position(&name).unwrap();
        let old = previous.and_then(|p| p.regions.get(&name));
        let mut counters = BTreeMap::new();
        let mut region_changed = Vec::new();
        let mut failed = false;
        for &(x, z) in positions.get(&region).into_iter().flatten() {
            match world.get_chunk(x, z) {
                Ok(Some(chunk)) => {
                    let key = format!("{x},{z}");
                    // A region with no usable baseline (including a failed
                    // initial read) starts quietly rather than marking it all.
                    if old.is_some()
                        && old.and_then(|r| r.counters.get(&key)) != Some(&chunk.last_update)
                    {
                        region_changed.push((x, z));
                    }
                    counters.insert(key, chunk.last_update);
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        x,
                        z,
                        "activity chunk unreadable; retry next update: {error:#}"
                    );
                    failed = true;
                }
            }
        }
        if failed || stamp(&directory.join(&name)).ok().as_ref() != current.get(&name) {
            tracing::warn!(
                region = name,
                "region unstable; retaining previous checkpoint"
            );
            continue;
        }
        changed.extend(region_changed);
        next.regions.insert(
            name.clone(),
            RegionState {
                stamp: current[&name].clone(),
                counters,
            },
        );
    }
    Ok((next, changed))
}

fn classify(
    changed: &BTreeSet<Position>,
    centers: impl Iterator<Item = Position>,
    limit: usize,
) -> Vec<(i32, i32, u8)> {
    let mut players = HashSet::new();
    'centers: for (x, z) in centers {
        for dx in -16..=16 {
            for dz in -16..=16 {
                if players.len() >= limit {
                    tracing::warn!("live overlay limit reached");
                    break 'centers;
                }
                if let (Some(x), Some(z)) = (x.checked_add(dx), z.checked_add(dz)) {
                    players.insert((x, z));
                }
            }
        }
    }
    let mut union: HashSet<_> = changed.iter().copied().collect();
    union.extend(&players);
    let mut output: BTreeSet<_> = players.iter().copied().collect();
    for &position in changed {
        if output.len() >= limit {
            break;
        }
        output.insert(position);
    }
    output
        .into_iter()
        .map(|(x, z)| {
            let kind = if players.contains(&(x, z)) {
                2
            } else if (-2..=2).all(|dx| {
                (-2..=2).all(|dz| {
                    x.checked_add(dx)
                        .zip(z.checked_add(dz))
                        .is_some_and(|p| union.contains(&p))
                })
            }) {
                1
            } else {
                0
            };
            (x, z, kind)
        })
        .collect()
}

// Bound depth before fastnbt visits even ignored inventory/entity tags.
fn validate_nbt(bytes: &[u8]) -> Result<()> {
    struct Scan<'a> {
        bytes: &'a [u8],
        pos: usize,
    }
    impl Scan<'_> {
        fn take(&mut self, n: usize) -> Result<&[u8]> {
            let end = self.pos.checked_add(n).context("NBT length overflow")?;
            ensure!(end <= self.bytes.len(), "truncated NBT");
            let bytes = &self.bytes[self.pos..end];
            self.pos = end;
            Ok(bytes)
        }
        fn byte(&mut self) -> Result<u8> {
            Ok(self.take(1)?[0])
        }
        fn string(&mut self) -> Result<()> {
            let n = u16::from_be_bytes(self.take(2)?.try_into()?) as usize;
            self.take(n)?;
            Ok(())
        }
        fn length(&mut self) -> Result<usize> {
            let n = i32::from_be_bytes(self.take(4)?.try_into()?);
            ensure!(n >= 0 && n as usize <= MAX_NBT, "invalid NBT length");
            Ok(n as usize)
        }
        fn value(&mut self, tag: u8, depth: usize) -> Result<()> {
            ensure!(depth <= 64, "NBT depth exceeds 64");
            match tag {
                1..=6 => {
                    self.take([0, 1, 2, 4, 8, 4, 8][tag as usize])?;
                }
                7 | 11 | 12 => {
                    let n = self.length()?;
                    self.take(
                        n.checked_mul(match tag {
                            7 => 1,
                            11 => 4,
                            _ => 8,
                        })
                        .context("NBT array overflow")?,
                    )?;
                }
                8 => self.string()?,
                9 => {
                    let tag = self.byte()?;
                    let n = self.length()?;
                    ensure!(tag <= 12 && (tag != 0 || n == 0), "invalid NBT list");
                    for _ in 0..n {
                        self.value(tag, depth + 1)?;
                    }
                }
                10 => loop {
                    let tag = self.byte()?;
                    if tag == 0 {
                        break;
                    }
                    self.string()?;
                    self.value(tag, depth + 1)?;
                },
                _ => anyhow::bail!("invalid NBT tag"),
            }
            Ok(())
        }
    }
    let mut scan = Scan { bytes, pos: 0 };
    ensure!(scan.byte()? == 10, "NBT root must be compound");
    scan.string()?;
    scan.value(10, 0)?;
    ensure!(scan.pos == bytes.len(), "trailing NBT bytes");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }
    #[test]
    fn parses_player_and_floors_negative_coordinates() {
        let bytes =
            fastnbt::to_bytes(&fastnbt::nbt!({"Dimension": -1i32, "Pos": [-0.5f64, 64.0, -16.1]}))
                .unwrap();
        let player = parse_player(gzip(&bytes).as_slice()).unwrap();
        assert_eq!(player.dimension, -1);
        assert_eq!(
            (
                chunk_coordinate(player.pos[0]),
                chunk_coordinate(player.pos[2])
            ),
            (-1, -2)
        );
        let invalid =
            fastnbt::to_bytes(&fastnbt::nbt!({"Dimension": 0i32, "Pos": [1f64,2.0]})).unwrap();
        assert!(parse_player(gzip(&invalid).as_slice()).is_err());
        assert!(parse_player(gzip(&vec![0; MAX_NBT + 1]).as_slice()).is_err());
    }
    #[test]
    fn activity_interior_border_and_player_priority() {
        let changed: BTreeSet<_> = (-3..=3)
            .flat_map(|x| (-3..=3).map(move |z| (x, z)))
            .collect();
        let result = classify(&changed, std::iter::empty(), MAX_OVERLAY);
        assert!(result.contains(&(0, 0, 1)));
        assert!(result.contains(&(3, 3, 0)));
        let result = classify(&changed, std::iter::once((0, 0)), MAX_OVERLAY);
        assert_eq!(result.len(), 1089);
        assert!(result.iter().all(|p| p.2 == 2));
        assert_eq!(classify(&changed, std::iter::once((0, 0)), 20).len(), 20);
    }
    #[test]
    fn local_heads_only() {
        assert!(safe_head("heads/alice.png"));
        for bad in [
            "javascript:alert(1)",
            "https://example.com/x",
            "//example.com/x",
            "../x",
            "heads/%2e%2e/x",
            "heads\\x",
            "/x",
        ] {
            assert!(!safe_head(bad));
        }
    }
    #[test]
    fn checkpoint_baseline_changes_unchanged_and_deletion() {
        fn region(path: &Path, tick: i64) {
            let raw = fastnbt::to_bytes(&fastnbt::nbt!({ "Level": {
                "xPos": 0i32, "zPos": 0i32, "LastUpdate": tick
            }}))
            .unwrap();
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(&raw).unwrap();
            let payload = encoder.finish().unwrap();
            let mut bytes = vec![0; 12288];
            bytes[..4].copy_from_slice(&[0, 0, 2, 1]);
            bytes[8192..8196].copy_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
            bytes[8196] = 2;
            bytes[8197..8197 + payload.len()].copy_from_slice(&payload);
            fs::write(path, bytes).unwrap();
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("r.0.0.mca");
        region(&path, 100);
        let (baseline, changed) = scan_activity(directory.path(), "test", None, 0).unwrap();
        assert!(changed.is_empty());
        assert_eq!(baseline.regions["r.0.0.mca"].counters["0,0"], 100);
        // Round-trip the same persistent format used across process restarts.
        let mut baseline: WorldState =
            serde_json::from_slice(&serde_json::to_vec(&baseline).unwrap()).unwrap();
        region(&path, 200);
        // Force a distinct old stamp even on coarse-resolution test filesystems.
        baseline.regions.get_mut("r.0.0.mca").unwrap().stamp.size = 0;
        let (next, changed) = scan_activity(directory.path(), "test", Some(&baseline), 0).unwrap();
        assert_eq!(changed, BTreeSet::from([(0, 0)]));
        let (_, changed) = scan_activity(directory.path(), "test", Some(&next), 0).unwrap();
        assert!(changed.is_empty());
        fs::remove_file(path).unwrap();
        let (next, changed) = scan_activity(directory.path(), "test", Some(&next), 0).unwrap();
        assert!(next.regions.is_empty());
        assert!(changed.is_empty());
    }
}
