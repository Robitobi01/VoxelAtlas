//! Incremental tiles, immutable content-addressed storage, and atomic publication.
use crate::{
    assets::Assets,
    codec::{TileEncoder, read_half},
    config::{Config, Encoding, MapConfig, Mode, View},
    progress::Progress,
    render,
    world::{Fingerprint, RegionFingerprint, World},
};
use anyhow::{Context, Result, bail};
use image::RgbaImage;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub generation: String,
    pub tile_size: u32,
    pub maps: Vec<MapManifest>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MapManifest {
    pub id: String,
    pub world: String,
    pub view: View,
    pub rotation: u8,
    pub mode: Mode,
    pub default_zoom: u8,
    pub bounds: [i64; 4],
    pub min_zoom: i32,
    pub max_zoom: i32,
    #[serde(skip)]
    pub tiles: BTreeMap<String, String>,
    #[serde(default)]
    pub tile_indexes: BTreeMap<String, String>,
}
#[derive(Default, Serialize, Deserialize)]
struct TileIndex {
    tiles: BTreeMap<String, String>,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    signature: String,
    #[serde(default)]
    regions: BTreeMap<String, BTreeMap<String, RegionFingerprint>>,
    #[serde(default)]
    layout_signature: String,
    #[serde(default)]
    map_signatures: BTreeMap<String, String>,
    #[serde(default)]
    render_chunks: BTreeMap<String, BTreeMap<String, Fingerprint>>,
}
#[derive(Debug, Default, Serialize)]
pub struct RenderStats {
    pub chunks_scanned: usize,
    pub chunks_changed: usize,
    pub base_tiles_rendered: usize,
    pub parent_tiles_rendered: usize,
    pub unchanged: bool,
    pub scan_seconds: f64,
    pub render_seconds: f64,
    /// Summed worker wall time (encoding + immutable tile writes), not CPU time.
    pub encode_worker_seconds: f64,
    pub pyramid_seconds: f64,
    pub encoded_tiles: u64,
    pub duplicate_encodes_avoided: u64,
    pub metadata_only_chunks: usize,
    pub skipped_corrupt_chunks: usize,
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_with(path, |file| {
        file.write_all(bytes)?;
        Ok(())
    })
}
fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_write_with(path, |file| {
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, value)?;
        writer.flush()?;
        Ok(())
    })
}

/// Large phase-local trees and worker scratch can otherwise remain resident in
/// glibc arenas after Rust drops them, consuming the cgroup's file-cache budget.
fn release_unused_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // SAFETY: glibc's thread-safe API only releases allocator-owned free
        // pages. No live Rust allocations are moved or invalidated.
        unsafe {
            malloc_trim(0);
        }
    }
}
fn atomic_write_with(path: &Path, write: impl FnOnce(&mut fs::File) -> Result<()>) -> Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().context("output has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        write(&mut file)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result.with_context(|| format!("publishing {}", path.display()))
}

// Kernel lock is automatically released on crash. It also protects publication and GC.
pub struct OutputLock(fs::File);
impl OutputLock {
    pub fn acquire(output: &Path) -> Result<Self> {
        fs::create_dir_all(output.join(".state"))?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(output.join(".state/lock"))?;
        file.try_lock()
            .context("another VoxelAtlas writer is running for this output")?;
        Ok(Self(file))
    }
}
impl Drop for OutputLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn tile_key(z: i32, x: i32, y: i32) -> String {
    format!("{z}/{x}/{y}")
}
fn parse_key(s: &str) -> Result<(i32, i32, i32)> {
    let mut parts = s.split('/');
    let (Some(z), Some(x), Some(y)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("invalid tile key: {s}");
    };
    if parts.next().is_some() {
        bail!("invalid tile key: {s}");
    }
    Ok((z.parse()?, x.parse()?, y.parse()?))
}
fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid content hash");
    }
    Ok(())
}
fn load_indexes(output: &Path, map: &mut MapManifest) -> Result<()> {
    for hash in map.tile_indexes.values() {
        validate_hash(hash)?;
        let index: TileIndex = serde_json::from_slice(&fs::read(
            output.join("indexes").join(format!("{hash}.json")),
        )?)?;
        map.tiles.extend(index.tiles);
    }
    Ok(())
}
fn store_indexes(output: &Path, map: &mut MapManifest) -> Result<()> {
    let mut shards = BTreeMap::<String, TileIndex>::new();
    for (key, hash) in std::mem::take(&mut map.tiles) {
        let (z, x, y) = parse_key(&key)?;
        shards
            .entry(tile_key(z, x.div_euclid(32), y.div_euclid(32)))
            .or_default()
            .tiles
            .insert(key, hash);
    }
    map.tile_indexes.clear();
    for (key, index) in shards {
        let bytes = serde_json::to_vec(&index)?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let path = output.join("indexes").join(format!("{hash}.json"));
        if !path.exists() {
            atomic_write(&path, &bytes)?;
        }
        map.tile_indexes.insert(key, hash);
    }
    Ok(())
}

/// Alpha-weighted 2x reduction: transparent texels must not introduce dark fringes.
/// Rows are walked directly rather than through bounds-checked pixel accessors;
/// the arithmetic is unchanged and still exactly integral.
pub fn reduce(image: &RgbaImage) -> RgbaImage {
    let (width, height) = (image.width() / 2, image.height() / 2);
    let mut out = RgbaImage::new(width, height);
    let source = image.as_chunks::<4>().0;
    let stride = image.width() as usize;
    debug_assert_eq!(source.len(), stride * image.height() as usize);
    for (y, row) in out
        .as_chunks_mut::<4>()
        .0
        .chunks_exact_mut(width.max(1) as usize)
        .enumerate()
        .take(height as usize)
    {
        let top = &source[y * 2 * stride..][..stride];
        let bottom = &source[(y * 2 + 1) * stride..][..stride];
        for (x, pixel) in row.iter_mut().enumerate() {
            let quad = [top[x * 2], top[x * 2 + 1], bottom[x * 2], bottom[x * 2 + 1]];
            let alpha: u32 = quad.iter().map(|p| u32::from(p[3])).sum();
            let mut rgb = [0u32; 3];
            for p in &quad {
                for (sum, &c) in rgb.iter_mut().zip(&p[..3]) {
                    *sum += u32::from(c) * u32::from(p[3]);
                }
            }
            let rgb = rgb.map(|v| v.checked_div(alpha).unwrap_or(0) as u8);
            *pixel = [rgb[0], rgb[1], rgb[2], ((alpha + 2) / 4) as u8];
        }
    }
    out
}

fn compose_parent(
    output: &Path,
    encoder: &TileEncoder,
    tiles: &BTreeMap<String, String>,
    zoom: i32,
    x: i32,
    y: i32,
    size: u32,
) -> Result<Option<String>> {
    let mut parent = RgbaImage::new(size, size);
    for dy in 0..2 {
        for dx in 0..2 {
            if let Some(hash) = tiles.get(&tile_key(zoom + 1, x * 2 + dx, y * 2 + dy)) {
                let child = read_half(output, hash, size)?;
                image::imageops::replace(
                    &mut parent,
                    &child,
                    dx as i64 * (size / 2) as i64,
                    dy as i64 * (size / 2) as i64,
                );
            }
        }
    }
    encoder.encode(&parent)
}
fn chunk_key(key: &str) -> Result<(i32, i32)> {
    let (x, z) = key
        .split_once(',')
        .context("invalid cached chunk position")?;
    Ok((x.parse()?, z.parse()?))
}
fn map_bounds(tiles: &BTreeMap<String, String>, size: u32) -> Result<[i64; 4]> {
    let mut bounds = [i64::MAX, i64::MAX, i64::MIN, i64::MIN];
    for key in tiles.keys() {
        let (z, x, y) = parse_key(key)?;
        if z != 0 {
            continue;
        }
        bounds[0] = bounds[0].min(x as i64 * size as i64);
        bounds[1] = bounds[1].min(y as i64 * size as i64);
        bounds[2] = bounds[2].max((x as i64 + 1) * size as i64);
        bounds[3] = bounds[3].max((y as i64 + 1) * size as i64);
    }
    if bounds[0] == i64::MAX {
        Ok([0, 0, size as i64, size as i64])
    } else {
        Ok(bounds)
    }
}
fn min_zoom(bounds: [i64; 4], size: u32) -> i32 {
    let mut z = 0;
    let mut scale = size as i64;
    while (bounds[2] - 1).div_euclid(scale) - bounds[0].div_euclid(scale) > 1
        || (bounds[3] - 1).div_euclid(scale) - bounds[1].div_euclid(scale) > 1
    {
        z -= 1;
        scale *= 2;
    }
    z
}
fn new_map(map: &MapConfig, rotation: u8) -> MapManifest {
    MapManifest {
        id: map.id.clone(),
        world: map.world.clone(),
        view: map.view,
        rotation,
        mode: map.mode,
        default_zoom: map.default_zoom,
        bounds: [0; 4],
        min_zoom: 0,
        max_zoom: 0,
        tiles: BTreeMap::new(),
        tile_indexes: BTreeMap::new(),
    }
}

pub fn render(config: &Config, force: bool) -> Result<RenderStats> {
    render_with_options(config, force, false)
}
pub fn render_with_options(config: &Config, force: bool, verify: bool) -> Result<RenderStats> {
    config.validate()?;
    let _lock = OutputLock::acquire(&config.output)?;
    let progress = Progress::new(config.workers, config.cache_mib);
    fs::create_dir_all(config.output.join("tiles"))?;
    fs::create_dir_all(config.output.join("indexes"))?;
    // Loading assets before changing output makes invalid asset configurations fail safely.
    let assets = Assets::load(&config.textures)?;
    if !config.output.join("current.json").exists() && !config.output.join("index.html").exists() {
        atomic_write(&config.output.join("index.html"), b"<!doctype html><html lang=en><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><meta http-equiv=refresh content=60><title>VoxelAtlas: awaiting first map</title><style>body{background:#101820;color:#e8eef5;font:18px system-ui;max-width:700px;margin:15vh auto;padding:24px}</style><h1>Map not published yet</h1><p>The first complete generation is not available yet. The viewer will replace this page when rendering finishes.</p><p>If rendering has stopped, check the VoxelAtlas service logs. This page refreshes every minute.</p></html>")?;
    }
    let mut signature = blake3::Hasher::new();
    // Resource/output-location changes must not force an expensive visual rebuild.
    signature.update(&config.tile_size.to_le_bytes());
    signature.update(&serde_json::to_vec(&config.encoding)?);
    signature.update(&[u8::from(config.skip_corrupt_chunks)]);
    signature.update(assets.fingerprint().as_bytes());
    signature.update(include_bytes!("render.rs"));
    signature.update(include_bytes!("assets.rs"));
    signature.update(include_bytes!("world.rs"));
    signature.update(include_bytes!("output.rs"));
    signature.update(include_bytes!("codec.rs"));
    signature.update(include_bytes!("quantize.rs"));
    signature.update(include_bytes!("../Cargo.lock"));
    let signature = signature.finalize().to_hex().to_string();
    let state_path = config.output.join(".state/index.json");
    let mut old_state: State = match fs::File::open(&state_path) {
        Ok(file) => serde_json::from_reader(std::io::BufReader::new(file))
            .context("invalid incremental state; use --force after removing damaged state")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(e) => return Err(e.into()),
    };
    let old_manifest: Option<Manifest> = match fs::read(config.output.join("current.json")) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).context("invalid current manifest")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let full = force
        || signature != old_state.signature
        || old_manifest.as_ref().is_none_or(|m| m.version != 2);
    let layout_signature = blake3::hash(&serde_json::to_vec(
        &serde_json::json!({"worlds":config.worlds,"maps":config.maps}),
    )?)
    .to_hex()
    .to_string();
    let mut map_signatures = BTreeMap::new();
    for map in &config.maps {
        for &rotation in &map.rotations {
            let value = serde_json::json!({"world":config.worlds[&map.world],"view":map.view,"rotation":rotation,"mode":map.mode,"water_opacity":map.water_opacity,"lighting_water_intensity":map.lighting_water_intensity});
            map_signatures.insert(
                format!("{}/{rotation}", map.id),
                blake3::hash(&serde_json::to_vec(&value)?)
                    .to_hex()
                    .to_string(),
            );
        }
    }
    let layout_changed = layout_signature != old_state.layout_signature
        || map_signatures != old_state.map_signatures;
    let mut render_chunks = std::mem::take(&mut old_state.render_chunks);
    if full {
        // A complete rebuild repopulates this cache; don't retain hashes for
        // deleted chunks or worlds that no longer contribute a view.
        render_chunks.clear();
    }
    render_chunks.retain(|id, _| config.worlds.contains_key(id));
    let mut state = State {
        signature,
        regions: BTreeMap::new(),
        layout_signature,
        map_signatures,
        render_chunks,
    };
    let mut worlds = BTreeMap::<String, Arc<World>>::new();
    let mut opened = BTreeMap::<PathBuf, Arc<World>>::new();
    // Divide a single decoded-chunk budget over distinct physical dimension directories.
    let directories: BTreeSet<_> = config
        .worlds
        .values()
        .map(|w| w.dimension.directory(&w.path))
        .collect();
    let per_world = config.cache_mib * 1024 * 1024 / directories.len().max(1);
    let mut stats = RenderStats::default();
    let scan_start = std::time::Instant::now();
    for (id, w) in &config.worlds {
        progress.stage(&format!("Indexing {id}"));
        let directory = w.dimension.directory(&w.path);
        let world = if let Some(world) = opened.get(&directory) {
            world.clone()
        } else {
            let world = Arc::new(
                World::open_with_options(&directory, per_world, config.skip_corrupt_chunks)
                    .with_context(|| format!("opening world {id}"))?,
            );
            opened.insert(directory.clone(), world.clone());
            world
        };
        // Aliases to the same dimension share the expensive payload fingerprint scan.
        let existing_id = worlds
            .iter()
            .find(|(_, other)| Arc::ptr_eq(other, &world))
            .map(|(id, _)| id.clone());
        let empty_cache = BTreeMap::new();
        let regions = if let Some(existing_id) = &existing_id {
            state.regions[existing_id].clone()
        } else {
            let bar = progress.bar(world.region_count(), "Scan regions");
            let result = world.fingerprints_cached_progress(
                old_state.regions.get(id).unwrap_or(&empty_cache),
                verify,
                |done, _| {
                    bar.set_position(done as u64);
                    progress.refresh();
                },
            );
            bar.finish_and_clear();
            result?
        };
        stats.chunks_scanned += regions.values().map(|r| r.chunks.len()).sum::<usize>();
        state.regions.insert(id.clone(), regions);
        worlds.insert(id.clone(), world);
    }
    let mut changed = BTreeMap::<String, BTreeSet<(i32, i32)>>::new();
    for id in config.worlds.keys() {
        if full {
            // A full view derives its work from the world's chunk index below;
            // retaining a second world-sized dirty tree adds no information.
            stats.chunks_changed += state.regions[id]
                .values()
                .map(|region| region.chunks.len())
                .sum::<usize>();
            changed.insert(id.clone(), BTreeSet::new());
            continue;
        }
        let now: BTreeMap<_, _> = state.regions[id]
            .values()
            .flat_map(|r| r.chunks.iter())
            .collect();
        let old: BTreeMap<_, _> = old_state
            .regions
            .get(id)
            .into_iter()
            .flat_map(|rs| rs.values())
            .flat_map(|r| r.chunks.iter())
            .collect();
        let mut keys: BTreeSet<_> = now.keys().copied().collect();
        keys.extend(old.keys().copied());
        let mut positions = BTreeSet::new();
        let render_hashes = state.render_chunks.entry(id.clone()).or_default();
        for key in keys {
            if full || old.get(key) != now.get(key) {
                let pos = chunk_key(key)?;
                if !now.contains_key(key) {
                    render_hashes.remove(key);
                    positions.insert(pos);
                } else if full {
                    // Hashes are harvested from normal raster reads below; do
                    // not predecode an entire world just to populate the index.
                    positions.insert(pos);
                } else {
                    let chunk = worlds[id]
                        .get_chunk(pos.0, pos.1)
                        .with_context(|| format!("unreadable chunk {id} {},{}", pos.0, pos.1))?;
                    let Some(chunk) = chunk else {
                        if !config.skip_corrupt_chunks {
                            bail!("indexed chunk disappeared");
                        }
                        render_hashes.remove(key);
                        positions.insert(pos);
                        continue;
                    };
                    let hash = chunk.render_digest();
                    if render_hashes.get(key) != Some(&hash) {
                        positions.insert(pos);
                    } else {
                        stats.metadata_only_chunks += 1;
                    }
                    render_hashes.insert(key.clone(), hash);
                }
            }
        }
        stats.chunks_changed += positions.len();
        changed.insert(id.clone(), positions);
    }
    stats.scan_seconds = scan_start.elapsed().as_secs_f64();
    tracing::info!(
        seconds = stats.scan_seconds,
        chunks = stats.chunks_scanned,
        changed = stats.chunks_changed,
        "World scan complete"
    );
    if !full && !layout_changed && changed.values().all(BTreeSet::is_empty) {
        for world in worlds.values() {
            world.check_consistent()?;
        }
        // Refresh stat-cache stamps without rewriting tiles or the public manifest.
        if state.regions != old_state.regions {
            atomic_write_json(&state_path, &state)?;
        }
        install_viewer(&config.output)?;
        stats.skipped_corrupt_chunks = opened.values().map(|world| world.skipped_chunks()).sum();
        stats.unchanged = true;
        tracing::info!("World content and settings unchanged; no output rewritten");
        return Ok(stats);
    }
    // Rasterization only needs the old per-view signatures, not duplicate
    // copies of every compressed chunk fingerprint from the previous run.
    old_state.regions.clear();
    release_unused_memory();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.workers)
        .build()?;
    let encoder = if config.encoding == Encoding::Indexed {
        TileEncoder::indexed(&config.output)
    } else if config.encoding == Encoding::Compact {
        TileEncoder::compact(&config.output, false)
    } else {
        TileEncoder::new(&config.output)
    };
    let compact_base =
        (config.encoding == Encoding::Compact).then(|| TileEncoder::compact(&config.output, true));
    let mut maps = Vec::new();
    for (map_index, map) in config.maps.iter().enumerate() {
        let world = &worlds[&map.world];
        let exclusions: Vec<_> = config.worlds[&map.world]
            .exclude
            .iter()
            .map(|s| crate::config::parse_exclusion(s))
            .collect::<Result<_>>()?;
        for &rotation in &map.rotations {
            progress.stage(&format!("{} · rotation {rotation}", map.id));
            tracing::info!(map=%map.id,rotation,"Rendering view");
            let key = format!("{}/{rotation}", map.id);
            let map_full = full
                || old_state.map_signatures.get(&key) != state.map_signatures.get(&key)
                || old_manifest.as_ref().is_none_or(|m| {
                    !m.maps
                        .iter()
                        .any(|m| m.id == map.id && m.rotation == rotation)
                });
            let mut manifest = if map_full {
                new_map(map, rotation)
            } else {
                old_manifest
                    .as_ref()
                    .and_then(|m| {
                        m.maps
                            .iter()
                            .find(|m| m.id == map.id && m.rotation == rotation)
                    })
                    .cloned()
                    .unwrap_or_else(|| new_map(map, rotation))
            };
            manifest.default_zoom = map.default_zoom;
            manifest.world = map.world.clone();
            if !map_full && changed[&map.world].is_empty() {
                maps.push(manifest);
                continue;
            }
            load_indexes(&config.output, &mut manifest)?;
            let mut dirty = BTreeSet::new();
            let changed_positions = if map_full {
                world.chunk_positions()
            } else {
                changed[&map.world].iter().copied().collect()
            };
            for (x, z) in changed_positions {
                dirty.extend(render::affected_tiles(
                    x,
                    z,
                    map.view,
                    rotation,
                    config.tile_size,
                ));
            }
            let bar = progress.bar(dirty.len(), "Render + WebP");
            let base_encoder = if map.view == View::Isometric {
                compact_base.as_ref().unwrap_or(&encoder)
            } else {
                &encoder
            };
            let render_start = std::time::Instant::now();
            let encode_micros = AtomicU64::new(0);
            let results: Vec<Result<_>> = pool.install(|| {
                dirty
                    .par_iter()
                    .map(|&(x, y)| {
                        let tile = render::render_tile(
                            world,
                            &assets,
                            map,
                            rotation,
                            &exclusions,
                            x,
                            y,
                            config.tile_size,
                        );
                        let encode_start = std::time::Instant::now();
                        let hash = base_encoder.encode(&tile)?;
                        encode_micros.fetch_add(
                            encode_start.elapsed().as_micros() as u64,
                            Ordering::Relaxed,
                        );
                        bar.inc(1);
                        progress.refresh();
                        Ok((x, y, hash))
                    })
                    .collect()
            });
            bar.finish_and_clear();
            stats.render_seconds += render_start.elapsed().as_secs_f64();
            stats.encode_worker_seconds +=
                encode_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
            for result in results {
                let (x, y, hash) = result?;
                let key = tile_key(0, x, y);
                if let Some(hash) = hash {
                    manifest.tiles.insert(key, hash);
                } else {
                    manifest.tiles.remove(&key);
                }
            }
            stats.base_tiles_rendered += dirty.len();
            release_unused_memory();
            manifest.bounds = map_bounds(&manifest.tiles, config.tile_size)?;
            let old_min_zoom = manifest.min_zoom;
            let pyramid_start = std::time::Instant::now();
            manifest.min_zoom = min_zoom(manifest.bounds, config.tile_size);
            // When the map grows, propagate unchanged edge tiles into newly needed levels too.
            for zoom in (manifest.min_zoom..0).rev() {
                let parents: BTreeSet<_> = if map_full || zoom < old_min_zoom {
                    manifest
                        .tiles
                        .keys()
                        .filter_map(|key| parse_key(key).ok())
                        .filter(|&(z, _, _)| z == zoom + 1)
                        .map(|(_, x, y)| (x.div_euclid(2), y.div_euclid(2)))
                        .collect()
                } else {
                    dirty
                        .iter()
                        .map(|&(x, y)| (x.div_euclid(2), y.div_euclid(2)))
                        .collect()
                };
                let bar = progress.bar(parents.len(), &format!("Zoom {zoom}"));
                let results: Vec<Result<_>> = pool.install(|| {
                    parents
                        .par_iter()
                        .map(|&(x, y)| {
                            let hash = compose_parent(
                                &config.output,
                                &encoder,
                                &manifest.tiles,
                                zoom,
                                x,
                                y,
                                config.tile_size,
                            )?;
                            bar.inc(1);
                            progress.refresh();
                            Ok((x, y, hash))
                        })
                        .collect()
                });
                bar.finish_and_clear();
                for result in results {
                    let (x, y, hash) = result?;
                    let key = tile_key(zoom, x, y);
                    if let Some(hash) = hash {
                        manifest.tiles.insert(key, hash);
                    } else {
                        manifest.tiles.remove(&key);
                    }
                }
                stats.parent_tiles_rendered += parents.len();
                dirty = parents;
            }
            stats.pyramid_seconds += pyramid_start.elapsed().as_secs_f64();
            manifest
                .tiles
                .retain(|key, _| parse_key(key).is_ok_and(|(z, _, _)| z >= manifest.min_zoom));
            progress.stage(&format!(
                "Indexing tiles · {} · rotation {rotation}",
                map.id
            ));
            store_indexes(&config.output, &mut manifest)?;
            release_unused_memory();
            maps.push(manifest);
        }
        if !config.maps[map_index + 1..]
            .iter()
            .any(|later| later.world == map.world)
        {
            changed.remove(&map.world);
        }
    }
    // Never publish holes caused by unreadable chunks or a changing source world.
    // The previous complete generation stays available if consistency fails.
    for world in opened.values() {
        world.check_consistent()?;
        let decoded = world.take_render_fingerprints();
        let mut aliases = worlds
            .iter()
            .filter(|(_, other)| Arc::ptr_eq(world, other))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if let Some(last) = aliases.pop() {
            for id in aliases {
                state
                    .render_chunks
                    .entry(id)
                    .or_default()
                    .extend(decoded.iter().map(|(k, v)| (k.clone(), *v)));
            }
            state.render_chunks.entry(last).or_default().extend(decoded);
        }
    }
    stats.skipped_corrupt_chunks = opened.values().map(|world| world.skipped_chunks()).sum();
    stats.encoded_tiles = encoder.encoded.load(Ordering::Relaxed);
    stats.duplicate_encodes_avoided = encoder.reused.load(Ordering::Relaxed);
    if let Some(base) = compact_base {
        stats.encoded_tiles += base.encoded.load(Ordering::Relaxed);
        stats.duplicate_encodes_avoided += base.reused.load(Ordering::Relaxed);
    }
    let mut manifest = Manifest {
        version: 2,
        generation: String::new(),
        tile_size: config.tile_size,
        maps,
    };
    manifest.generation = blake3::hash(&serde_json::to_vec(&manifest)?)
        .to_hex()
        .to_string();
    let bytes = serde_json::to_vec(&manifest)?;
    progress.stage("Publishing generation");
    install_viewer(&config.output)?;
    atomic_write(
        &config
            .output
            .join("generations")
            .join(format!("{}.json", manifest.generation)),
        &bytes,
    )?;
    atomic_write(&config.output.join("current.json"), &bytes)?;
    atomic_write_json(&state_path, &state)?;
    Ok(stats)
}
fn install_viewer(output: &Path) -> Result<()> {
    for (name, bytes) in [
        ("index.html", include_bytes!("../web/index.html").as_slice()),
        ("app.js", include_bytes!("../web/app.js").as_slice()),
        ("style.css", include_bytes!("../web/style.css").as_slice()),
    ] {
        let path = output.join(name);
        if fs::read(&path).ok().as_deref() != Some(bytes) {
            atomic_write(&path, bytes)?;
        }
    }
    Ok(())
}
pub fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Explicit garbage collection only: rendering never deletes published generations.
pub fn gc(output: &Path, keep: usize, grace_seconds: u64) -> Result<usize> {
    if keep == 0 {
        bail!("keep must be at least one generation");
    }
    let _lock = OutputLock::acquire(output)?;
    let current: Manifest = serde_json::from_slice(&fs::read(output.join("current.json"))?)?;
    let mut generations = Vec::new();
    for entry in fs::read_dir(output.join("generations"))? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|s| s == "json") {
            let modified = entry.metadata()?.modified()?;
            generations.push((modified, entry.path()));
        }
    }
    generations.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut indexes: BTreeSet<_> = current
        .maps
        .iter()
        .flat_map(|m| m.tile_indexes.values().cloned())
        .collect();
    let mut removed = 0;
    for (index, (modified, path)) in generations.iter().enumerate() {
        let young = SystemTime::now()
            .duration_since(*modified)
            .unwrap_or_default()
            .as_secs()
            < grace_seconds;
        let manifest: Manifest = serde_json::from_slice(&fs::read(path)?)?;
        if index < keep || young || manifest.generation == current.generation {
            indexes.extend(
                manifest
                    .maps
                    .into_iter()
                    .flat_map(|m| m.tile_indexes.into_values()),
            );
        } else {
            fs::remove_file(path)?;
        }
    }
    let mut reachable = BTreeSet::new();
    for hash in &indexes {
        validate_hash(hash)?;
        let index: TileIndex = serde_json::from_slice(&fs::read(
            output.join("indexes").join(format!("{hash}.json")),
        )?)?;
        reachable.extend(index.tiles.into_values());
    }
    for entry in fs::read_dir(output.join("indexes"))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|s| s != "json") {
            continue;
        }
        let hash = path.file_stem().unwrap_or_default().to_string_lossy();
        let age = SystemTime::now()
            .duration_since(entry.metadata()?.modified()?)
            .unwrap_or_default()
            .as_secs();
        if !indexes.contains(hash.as_ref()) && age >= grace_seconds {
            fs::remove_file(path)?;
        }
    }
    for entry in fs::read_dir(output.join("tiles"))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|s| s != "webp") {
            continue;
        }
        let hash = path.file_stem().unwrap_or_default().to_string_lossy();
        let age = SystemTime::now()
            .duration_since(entry.metadata()?.modified()?)
            .unwrap_or_default()
            .as_secs();
        if !reachable.contains(hash.as_ref()) && age >= grace_seconds {
            fs::remove_file(path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn alpha_downsample() {
        let mut img = RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([255, 100, 50, 255]));
        assert_eq!(reduce(&img).get_pixel(0, 0).0, [255, 100, 50, 64]);
    }
    #[test]
    fn signed_tile_keys() {
        assert_eq!(parse_key("-2/-3/1").unwrap(), (-2, -3, 1));
        assert_eq!((-3i32).div_euclid(2), -2);
    }
    #[test]
    fn atomic_publication() {
        let dir = tempfile::tempdir().unwrap();
        atomic_write(&dir.path().join("current.json"), b"first").unwrap();
        atomic_write(&dir.path().join("current.json"), b"second").unwrap();
        assert_eq!(
            fs::read(dir.path().join("current.json")).unwrap(),
            b"second"
        );
    }
    #[test]
    fn lock_exclusion() {
        let dir = tempfile::tempdir().unwrap();
        let first = OutputLock::acquire(dir.path()).unwrap();
        assert!(OutputLock::acquire(dir.path()).is_err());
        drop(first);
        assert!(OutputLock::acquire(dir.path()).is_ok());
    }
}
