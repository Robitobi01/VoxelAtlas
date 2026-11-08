//! Read-only Java Anvil (pre-flattening, through 1.12.2) world access.
use anyhow::{Context, Result, bail, ensure};
use fastnbt::ByteArray;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::File,
    io::Read,
    os::unix::fs::{FileExt, MetadataExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const SECTOR: u64 = 4096;
const MAX_NBT: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default)]
pub struct Block {
    pub id: u16,
    pub data: u8,
    pub block_light: u8,
    pub sky_light: u8,
    pub biome: u8,
}

pub struct Chunk {
    pub x: i32,
    pub z: i32,
    /// Saved Level.LastUpdate tick; not an authoritative activity indicator.
    pub last_update: i64,
    sections: [Option<Section>; 16],
    biomes: [u8; 256],
    // Actual non-air occupancy, including transparent blocks and extended IDs.
    // Legacy HeightMap is unsuitable: it can omit glass, foliage, and water.
    column_heights: [u16; 256],
    column_sections: [u16; 256],
}

impl Chunk {
    /// Conservative fingerprint of every input currently sampled by the rasterizer.
    /// Save ticks, entities, and other ignored NBT must not invalidate pixels.
    /// Any future renderer inputs must be added here (and version the renderer's
    /// source fingerprint). Optional zero arrays may cause harmless false misses.
    pub fn render_fingerprint(&self) -> String {
        self.render_hash().to_hex().to_string()
    }

    fn render_hash(&self) -> blake3::Hash {
        let mut hash = blake3::Hasher::new();
        hash.update(b"voxelatlas-chunk-render-inputs-v1");
        hash.update(&self.x.to_le_bytes());
        hash.update(&self.z.to_le_bytes());
        hash.update(&self.biomes);
        let mut bytes = [0u8; 4096];
        for (y, section) in self.sections.iter().enumerate() {
            hash.update(&[y as u8, u8::from(section.is_some())]);
            if let Some(section) = section {
                // ByteArray stores signed bytes; preserve their exact bit pattern.
                for (to, &from) in bytes.iter_mut().zip(section.blocks.iter()) {
                    *to = from as u8;
                }
                hash.update(&bytes);
                for array in [
                    &section.add,
                    &section.data,
                    &section.block_light,
                    &section.sky_light,
                ] {
                    hash.update(&[u8::from(array.is_some())]);
                    if let Some(array) = array {
                        for (to, &from) in bytes.iter_mut().zip(array.iter()) {
                            *to = from as u8;
                        }
                        hash.update(&bytes[..array.len()]);
                    }
                }
            }
        }
        hash.finalize()
    }

    pub fn highest_block_y(&self, x: u8, z: u8) -> Option<u8> {
        if x >= 16 || z >= 16 {
            return None;
        }
        self.column_heights[z as usize * 16 + x as usize]
            .checked_sub(1)
            .map(|y| y as u8)
    }

    pub(crate) fn column_section_present(&self, x: u8, y: u8, z: u8) -> bool {
        self.column_sections[z as usize * 16 + x as usize] & (1 << (y / 16)) != 0
    }

    pub fn block(&self, x: u8, y: u8, z: u8) -> Block {
        if x >= 16 || z >= 16 {
            return Block::default();
        }
        let biome = self.biomes[z as usize * 16 + x as usize];
        let Some(section) = &self.sections[y as usize / 16] else {
            return Block {
                biome,
                sky_light: 15,
                ..Block::default()
            };
        };
        let i = (y as usize % 16) * 256 + z as usize * 16 + x as usize;
        Block {
            id: section.blocks[i] as u8 as u16 | ((nibble(&section.add, i) as u16) << 8),
            data: nibble(&section.data, i),
            block_light: nibble(&section.block_light, i),
            sky_light: nibble(&section.sky_light, i),
            biome,
        }
    }

    fn memory_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self
                .sections
                .iter()
                .flatten()
                .map(|s| {
                    s.blocks.len()
                        + [&s.add, &s.data, &s.block_light, &s.sky_light]
                            .iter()
                            .map(|a| a.as_ref().map_or(0, |a| a.len()))
                            .sum::<usize>()
                })
                .sum::<usize>()
    }
}

fn nibble(array: &Option<ByteArray>, index: usize) -> u8 {
    array
        .as_ref()
        .map_or(0, |a| ((a[index / 2] as u8) >> ((index % 2) * 4)) & 15)
}

#[derive(Deserialize)]
struct Root {
    #[serde(rename = "DataVersion")]
    data_version: Option<i32>,
    #[serde(rename = "Level")]
    level: Level,
}
#[derive(Deserialize)]
struct Level {
    #[serde(rename = "xPos")]
    x: i32,
    #[serde(rename = "zPos")]
    z: i32,
    #[serde(rename = "LastUpdate", default)]
    last_update: i64,
    #[serde(rename = "Sections", default)]
    sections: Vec<Section>,
    #[serde(rename = "Biomes")]
    biomes: Option<ByteArray>,
}
#[derive(Deserialize)]
struct Section {
    #[serde(rename = "Y")]
    y: i8,
    #[serde(rename = "Blocks")]
    blocks: ByteArray,
    #[serde(rename = "Add")]
    add: Option<ByteArray>,
    #[serde(rename = "Data")]
    data: Option<ByteArray>,
    #[serde(rename = "BlockLight")]
    block_light: Option<ByteArray>,
    #[serde(rename = "SkyLight")]
    sky_light: Option<ByteArray>,
}

struct Location {
    region: String,
    offset: u64,
    sectors: u8,
    invalid: Option<String>,
}

/// Persistent per-region fingerprint cache. This is a filesystem-change cache,
/// not an integrity guarantee: use `verify=true` to distrust saved chunk hashes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegionFingerprint {
    pub chunks: BTreeMap<String, String>,
    stamp: RegionStamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RegionStamp {
    dev: u64,
    inode: u64,
    size: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}
impl RegionStamp {
    fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            mtime: metadata.mtime(),
            mtime_ns: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_ns: metadata.ctime_nsec(),
        }
    }
}

struct Region {
    path: PathBuf,
    stamp: RegionStamp,
    positions: Vec<(i32, i32)>,
}
impl Region {
    fn check_stamp(&self) -> Result<()> {
        let path = RegionStamp::from_metadata(&std::fs::metadata(&self.path)?);
        ensure!(
            path == self.stamp,
            "region {} changed since open or during hashing; reopen world and retry",
            self.path.display()
        );
        Ok(())
    }

    // Check the descriptor as well as the path to catch atomic replacements.
    fn check_file(&self, file: &File) -> Result<()> {
        ensure!(
            RegionStamp::from_metadata(&file.metadata()?) == self.stamp,
            "region {} changed since open; reopen world and retry",
            self.path.display()
        );
        self.check_stamp()
    }
}
struct Cached {
    chunk: Arc<Chunk>,
    used: u128,
    bytes: usize,
}
#[derive(Default)]
struct Cache {
    chunks: HashMap<(i32, i32), Cached>,
    failures: HashMap<(i32, i32), String>,
    bytes: usize,
    clock: u128,
    lru: BTreeSet<(u128, (i32, i32))>,
    warned: HashSet<(i32, i32)>,
    // Small metadata retained after decoded-array eviction, at most one 32-byte
    // digest (plus key/tree overhead) per indexed chunk, outside array budget.
    render_hashes: BTreeMap<(i32, i32), blake3::Hash>,
    skipped: HashSet<(i32, i32)>,
}

impl Cache {
    fn hit(&mut self, key: (i32, i32)) -> Option<Arc<Chunk>> {
        let hit = self.chunks.get_mut(&key)?;
        self.lru.remove(&(hit.used, key));
        self.clock += 1;
        hit.used = self.clock;
        self.lru.insert((hit.used, key));
        Some(hit.chunk.clone())
    }
}

fn region_coordinates(name: &str) -> Option<(i32, i32)> {
    let parts: Vec<_> = name.split('.').collect();
    if parts.len() != 4 || parts[0] != "r" || parts[3] != "mca" {
        return None;
    }
    Some((parts[1].parse().ok()?, parts[2].parse().ok()?))
}

pub struct World {
    region_directory: PathBuf,
    locations: BTreeMap<(i32, i32), Location>,
    regions: BTreeMap<String, Region>,
    cache: Mutex<Cache>,
    cache_bytes: usize,
    skip_corrupt_chunks: bool,
    skipped_allocations: usize,
}

#[derive(Debug)]
struct CorruptChunk(String);
impl std::fmt::Display for CorruptChunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for CorruptChunk {}

#[derive(Debug)]
struct UnsupportedVersion;
impl std::fmt::Display for UnsupportedVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("chunk DataVersion exceeds Minecraft Java 1.12.2 (1343)")
    }
}
impl std::error::Error for UnsupportedVersion {}

impl World {
    /// Header metadata is a snapshot. Do not modify region files during a render.
    pub fn open(region_directory: &Path, cache_bytes: usize) -> Result<Self> {
        Self::open_with_options(region_directory, cache_bytes, false)
    }

    pub fn open_with_options(
        region_directory: &Path,
        cache_bytes: usize,
        skip_corrupt_chunks: bool,
    ) -> Result<Self> {
        let mut locations: BTreeMap<(i32, i32), Location> = BTreeMap::new();
        let mut regions = BTreeMap::new();
        for entry in std::fs::read_dir(region_directory)
            .with_context(|| format!("reading {}", region_directory.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some((rx, rz)) = region_coordinates(name) else {
                continue;
            };
            let file = File::open(entry.path())?;
            let stamp = RegionStamp::from_metadata(&file.metadata()?);
            let size = stamp.size;
            let mut region = Region {
                path: entry.path(),
                stamp,
                positions: Vec::new(),
            };
            let mut header = [0; 8192];
            if skip_corrupt_chunks && size < header.len() as u64 {
                region.check_file(&file)?;
                tracing::warn!(region = %entry.path().display(), "Skipping corrupt region: truncated header; source file is unchanged");
                regions.insert(name.to_owned(), region);
                continue;
            }
            file.read_exact_at(&mut header, 0)
                .with_context(|| format!("reading region header {}", entry.path().display()))?;
            let mut occupied = HashMap::new();
            for i in 0..1024 {
                let loc = &header[i * 4..i * 4 + 4];
                let sector = u32::from_be_bytes([0, loc[0], loc[1], loc[2]]) as u64;
                let sectors = loc[3];
                if sector == 0 && sectors == 0 {
                    continue;
                }
                let x = i64::from(rx) * 32 + (i % 32) as i64;
                let z = i64::from(rz) * 32 + (i / 32) as i64;
                let (Ok(x), Ok(z)) = (i32::try_from(x), i32::try_from(z)) else {
                    bail!("region coordinates overflow: {name}")
                };
                let mut invalid = None;
                if sector < 2 || sectors == 0 || (sector + sectors as u64) * SECTOR > size {
                    invalid = Some("chunk sector allocation outside region file".into());
                } else {
                    for s in sector..sector + sectors as u64 {
                        if let Some(previous) = occupied.insert(s, (x, z)) {
                            invalid = Some("overlapping chunk sector allocation".into());
                            if let Some(previous) = locations.get_mut(&previous) {
                                previous.invalid = invalid.clone();
                            }
                        }
                    }
                }
                ensure!(
                    !locations.contains_key(&(x, z)),
                    "duplicate region chunk {x},{z}"
                );
                locations.insert(
                    (x, z),
                    Location {
                        region: name.to_owned(),
                        offset: sector * SECTOR,
                        sectors,
                        invalid,
                    },
                );
                region.positions.push((x, z));
            }
            region.check_file(&file)?;
            regions.insert(name.to_owned(), region);
        }
        let mut rejected = BTreeMap::<String, usize>::new();
        if skip_corrupt_chunks {
            locations.retain(|_, location| {
                if location.invalid.is_some() {
                    *rejected.entry(location.region.clone()).or_default() += 1;
                    false
                } else {
                    true
                }
            });
            for (name, count) in &rejected {
                let region = regions.get_mut(name).unwrap();
                region
                    .positions
                    .retain(|position| locations.contains_key(position));
                tracing::warn!(region = %region.path.display(), skipped_chunks = count,
                    "Skipping corrupt chunk sector allocations; source file is unchanged");
            }
        }
        let world = Self {
            region_directory: region_directory.to_owned(),
            locations,
            regions,
            cache: Mutex::new(Cache::default()),
            cache_bytes,
            skip_corrupt_chunks,
            skipped_allocations: rejected.values().sum(),
        };
        world.check_consistent()?;
        Ok(world)
    }

    /// Check before publishing output, after all readers have finished. Detects
    /// region additions/removals/replacements/changes and any recorded chunk
    /// read or decode failure, including errors that `block` rendered as air.
    /// This does not decode unrequested chunks or protect against later writes.
    pub fn check_consistent(&self) -> Result<()> {
        let mut names = BTreeSet::new();
        for entry in std::fs::read_dir(&self.region_directory)? {
            let name = entry?.file_name();
            if let Some(name) = name
                .to_str()
                .filter(|name| region_coordinates(name).is_some())
            {
                names.insert(name.to_owned());
            }
        }
        ensure!(
            names.len() == self.regions.len()
                && self.regions.keys().all(|name| names.contains(name)),
            "region file list changed since open; reopen world and retry"
        );
        for region in self.regions.values() {
            region.check_stamp()?;
        }
        let cache = self.cache.lock().unwrap();
        if let Some(((x, z), error)) = cache.failures.iter().next() {
            bail!("unreadable chunk {x},{z}: {error}");
        }
        Ok(())
    }

    pub fn chunk_positions(&self) -> Vec<(i32, i32)> {
        self.locations.keys().copied().collect()
    }

    pub fn skipped_chunks(&self) -> usize {
        self.skipped_allocations + self.cache.lock().unwrap().skipped.len()
    }

    fn skip_corruption(&self, x: i32, z: i32, error: &anyhow::Error) -> bool {
        if self.skip_corrupt_chunks && error.is::<CorruptChunk>() {
            if self.cache.lock().unwrap().skipped.insert((x, z)) {
                tracing::warn!(chunk_x = x, chunk_z = z, region = %self.regions[&self.locations[&(x,z)].region].path.display(),
                    "Skipping corrupt chunk; source file is unchanged: {error:#}");
            }
            true
        } else {
            false
        }
    }

    /// Successfully decoded render-input hashes, including evicted chunks.
    /// Does not decode anything. Compact digest metadata is bounded by indexed
    /// world chunks, not by the decoded-array cache budget. Hex strings are only
    /// allocated for this snapshot; workers hash outside the cache mutex.
    pub fn decoded_render_fingerprints(&self) -> BTreeMap<String, String> {
        self.cache
            .lock()
            .unwrap()
            .render_hashes
            .iter()
            .map(|(&(x, z), hash)| (format!("{x},{z}"), hash.to_hex().to_string()))
            .collect()
    }

    pub fn block(&self, x: i32, y: i32, z: i32) -> Block {
        if !(0..256).contains(&y) {
            return Block::default();
        }
        let key = (x.div_euclid(16), z.div_euclid(16));
        match self.get_chunk(key.0, key.1) {
            Ok(Some(chunk)) => chunk.block(x.rem_euclid(16) as u8, y as u8, z.rem_euclid(16) as u8),
            Ok(None) => Block::default(),
            Err(error) => {
                if self.cache.lock().unwrap().warned.insert(key) {
                    tracing::warn!(
                        chunk_x = key.0,
                        chunk_z = key.1,
                        "unreadable chunk: {error:#}"
                    );
                }
                Block::default()
            }
        }
    }

    /// Budget counts decoded arrays and inline chunk storage, not allocator/Arc
    /// overhead, metadata, or chunks still held by callers after eviction.
    /// Misses decode outside the cache lock: each concurrent caller may hold
    /// a compressed payload (at most 255 sectors), up to 16 MiB + 1 byte of NBT
    /// and its decoded arrays. Duplicate concurrent misses are deduplicated on
    /// insertion. No file handles are retained: at most one per active reader,
    /// plus one per concurrent region scan (not one per region in the world).
    pub fn get_chunk(&self, x: i32, z: i32) -> Result<Option<Arc<Chunk>>> {
        let Some(location) = self.locations.get(&(x, z)) else {
            return Ok(None);
        };
        let mut cache = self.cache.lock().unwrap();
        if cache.skipped.contains(&(x, z)) {
            return Ok(None);
        }
        if let Some(hit) = cache.hit((x, z)) {
            return Ok(Some(hit));
        }
        if let Some(error) = cache.failures.get(&(x, z)) {
            bail!("{error}");
        }
        drop(cache);
        let decoded = (|| {
            let region = &self.regions[&location.region];
            let (compression, payload) = {
                let file = File::open(&region.path)?;
                region.check_file(&file)?;
                let payload = read_payload(location, &file)?;
                region.check_file(&file)?;
                payload
            };
            decode_chunk(compression, &payload, x, z).map_err(|error| {
                if error.is::<UnsupportedVersion>() {
                    error
                } else {
                    anyhow::Error::new(CorruptChunk(format!("decoding chunk {x},{z}: {error:#}")))
                }
            })
        })();
        if let Err(error) = &decoded
            && self.skip_corruption(x, z, error)
        {
            return Ok(None);
        }
        let decoded = decoded.map(|chunk| {
            let hash = chunk.render_hash();
            (chunk, hash)
        });
        let mut cache = self.cache.lock().unwrap();
        let chunk = match decoded {
            Ok((chunk, hash)) => {
                cache.render_hashes.insert((x, z), hash);
                Arc::new(chunk)
            }
            Err(error) => {
                cache.failures.insert((x, z), format!("{error:#}"));
                return Err(error);
            }
        };
        if let Some(error) = cache.failures.get(&(x, z)) {
            bail!("{error}");
        }
        if let Some(hit) = cache.hit((x, z)) {
            return Ok(Some(hit));
        }
        let bytes = chunk.memory_bytes();
        if bytes <= self.cache_bytes {
            while cache.bytes > self.cache_bytes - bytes {
                let (_, key) = cache.lru.pop_first().unwrap();
                let old = cache.chunks.remove(&key).unwrap();
                cache.bytes -= old.bytes;
            }
            cache.bytes += bytes;
            cache.clock += 1;
            let clock = cache.clock;
            cache.lru.insert((clock, (x, z)));
            cache.chunks.insert(
                (x, z),
                Cached {
                    chunk: chunk.clone(),
                    used: clock,
                    bytes,
                },
            );
        }
        Ok(Some(chunk))
    }

    /// Fully rehash compressed chunk payloads and their compression type.
    pub fn fingerprints(&self) -> Result<BTreeMap<String, String>> {
        Ok(self
            .fingerprints_cached(&BTreeMap::new(), true)?
            .into_values()
            .flat_map(|region| region.chunks)
            .collect())
    }

    /// Reuse unchanged regions by Linux device/inode/size/mtime/ctime (including
    /// nanoseconds). `verify` always rereads payloads. Only one compressed chunk
    /// is held at a time; no region payload buffers are retained. Headers remain
    /// the snapshot indexed by open; changes require reopening rather than
    /// hashing new bytes with stale offsets. Saved caches are scoped per world.
    pub fn fingerprints_cached(
        &self,
        previous: &BTreeMap<String, RegionFingerprint>,
        verify: bool,
    ) -> Result<BTreeMap<String, RegionFingerprint>> {
        self.fingerprints_cached_progress(previous, verify, |_, _| {})
    }

    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Scan callback reports completed/total regions; it never runs on a worker.
    pub fn fingerprints_cached_progress(
        &self,
        previous: &BTreeMap<String, RegionFingerprint>,
        verify: bool,
        progress: impl Fn(usize, usize),
    ) -> Result<BTreeMap<String, RegionFingerprint>> {
        let mut result = BTreeMap::new();
        for (index, (name, region)) in self.regions.iter().enumerate() {
            region.check_stamp()?;
            let reusable = previous.get(name).filter(|old| {
                !verify
                    && old.stamp == region.stamp
                    && old.chunks.len() == region.positions.len()
                    && region
                        .positions
                        .iter()
                        .all(|(x, z)| old.chunks.contains_key(&format!("{x},{z}")))
            });
            let fingerprint = if let Some(old) = reusable {
                old.clone()
            } else {
                let file = File::open(&region.path)?;
                region.check_file(&file)?;
                let mut chunks = BTreeMap::new();
                for &(x, z) in &region.positions {
                    let (compression, payload) = match read_payload(&self.locations[&(x, z)], &file)
                    {
                        Ok(payload) => payload,
                        Err(error) if self.skip_corruption(x, z, &error) => continue,
                        Err(error) => {
                            return Err(error)
                                .with_context(|| format!("hashing chunk {x},{z} in {name}"));
                        }
                    };
                    chunks.insert(format!("{x},{z}"), payload_hash(compression, &payload));
                }
                region.check_file(&file)?;
                RegionFingerprint {
                    chunks,
                    stamp: region.stamp.clone(),
                }
            };
            region.check_stamp()?;
            result.insert(name.clone(), fingerprint);
            progress(index + 1, self.regions.len());
        }
        // Also catch an earlier region changing while a later one was hashed.
        self.check_consistent()?;
        Ok(result)
    }
}

fn payload_hash(compression: u8, payload: &[u8]) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(&[compression]);
    hash.update(payload);
    hash.finalize().to_hex().to_string()
}

fn read_payload(location: &Location, file: &File) -> Result<(u8, Vec<u8>)> {
    if let Some(error) = &location.invalid {
        bail!(CorruptChunk(error.clone()));
    }
    let mut header = [0; 5];
    file.read_exact_at(&mut header, location.offset)?;
    let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
    ensure!(
        length > 1 && length + 4 <= location.sectors as usize * SECTOR as usize,
        CorruptChunk(format!("invalid chunk payload length {length}"))
    );
    ensure!(
        matches!(header[4], 1 | 2),
        "unsupported chunk compression {}",
        header[4]
    );
    let mut payload = vec![0; length - 1];
    file.read_exact_at(&mut payload, location.offset + 5)?;
    Ok((header[4], payload))
}

fn decode_chunk(compression: u8, payload: &[u8], x: i32, z: i32) -> Result<Chunk> {
    let decoder: Box<dyn Read + '_> = match compression {
        1 => Box::new(flate2::read::GzDecoder::new(payload)),
        2 => Box::new(flate2::read::ZlibDecoder::new(payload)),
        _ => bail!("unsupported chunk compression {compression}"),
    };
    let mut nbt = Vec::new();
    decoder.take(MAX_NBT as u64 + 1).read_to_end(&mut nbt)?;
    ensure!(nbt.len() <= MAX_NBT, "decompressed chunk exceeds 16 MiB");
    validate_nbt(&nbt)?;
    let root: Root =
        fastnbt::from_bytes_with_opts(&nbt, fastnbt::DeOpts::new().max_seq_len(MAX_NBT))?;
    ensure!(
        root.data_version.is_none_or(|v| v <= 1343),
        UnsupportedVersion
    );
    ensure!(
        root.level.x == x && root.level.z == z,
        "chunk coordinates do not match region index"
    );
    let mut chunk = Chunk {
        x,
        z,
        last_update: root.level.last_update,
        sections: std::array::from_fn(|_| None),
        biomes: [0; 256],
        column_heights: [0; 256],
        column_sections: [0; 256],
    };
    if let Some(biomes) = root.level.biomes {
        ensure!(biomes.len() == 256, "invalid Biomes array length");
        for (to, from) in chunk.biomes.iter_mut().zip(biomes.iter()) {
            *to = *from as u8;
        }
    }
    for mut section in root.level.sections {
        ensure!((0..16).contains(&section.y), "invalid section Y");
        let y = section.y as usize;
        ensure!(chunk.sections[y].is_none(), "duplicate section Y");
        ensure!(section.blocks.len() == 4096, "invalid Blocks array length");
        for array in [
            &section.add,
            &section.data,
            &section.block_light,
            &section.sky_light,
        ]
        .into_iter()
        .flatten()
        {
            ensure!(array.len() == 2048, "invalid nibble array length");
        }
        // Absent nibble arrays read as zero. Drop redundant saved zero arrays
        // without changing block attributes, including air-section lighting.
        for array in [
            &mut section.add,
            &mut section.data,
            &mut section.block_light,
            &mut section.sky_light,
        ] {
            if array.as_ref().is_some_and(|a| a.iter().all(|&b| b == 0)) {
                *array = None;
            }
        }
        for local_y in 0..16 {
            for column in 0..256 {
                let i = local_y * 256 + column;
                if section.blocks[i] != 0 || nibble(&section.add, i) != 0 {
                    chunk.column_heights[column] =
                        chunk.column_heights[column].max((y * 16 + local_y + 1) as u16);
                    chunk.column_sections[column] |= 1 << y;
                }
            }
        }
        chunk.sections[y] = Some(section);
    }
    Ok(chunk)
}

// fastnbt has no recursion-limit option. Preflight all tags (including ignored
// entities) with a depth cap before allowing its serde decoder to recurse.
fn validate_nbt(bytes: &[u8]) -> Result<()> {
    struct Scan<'a> {
        bytes: &'a [u8],
        pos: usize,
    }
    impl Scan<'_> {
        fn take(&mut self, count: usize) -> Result<&[u8]> {
            let end = self.pos.checked_add(count).context("NBT length overflow")?;
            ensure!(end <= self.bytes.len(), "truncated NBT");
            let value = &self.bytes[self.pos..end];
            self.pos = end;
            Ok(value)
        }
        fn byte(&mut self) -> Result<u8> {
            Ok(self.take(1)?[0])
        }
        fn string(&mut self) -> Result<()> {
            let len = u16::from_be_bytes(self.take(2)?.try_into()?) as usize;
            self.take(len)?;
            Ok(())
        }
        fn length(&mut self) -> Result<usize> {
            let len = i32::from_be_bytes(self.take(4)?.try_into()?);
            ensure!(
                len >= 0 && len as usize <= MAX_NBT,
                "invalid NBT sequence length"
            );
            Ok(len as usize)
        }
        fn value(&mut self, tag: u8, depth: usize) -> Result<()> {
            ensure!(depth <= 64, "NBT nesting exceeds 64 levels");
            match tag {
                1..=6 => {
                    self.take([0, 1, 2, 4, 8, 4, 8][tag as usize])?;
                }
                7 | 11 | 12 => {
                    let len = self.length()?;
                    let width = match tag {
                        7 => 1,
                        11 => 4,
                        _ => 8,
                    };
                    self.take(len.checked_mul(width).context("NBT array overflow")?)?;
                }
                8 => self.string()?,
                9 => {
                    let element = self.byte()?;
                    let count = self.length()?;
                    ensure!(
                        element <= 12 && (element != 0 || count == 0),
                        "invalid NBT list tag"
                    );
                    for _ in 0..count {
                        self.value(element, depth + 1)?;
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
                _ => bail!("invalid NBT tag {tag}"),
            }
            Ok(())
        }
    }
    let mut scan = Scan { bytes, pos: 0 };
    ensure!(scan.byte()? == 10, "NBT root must be compound");
    scan.string()?;
    scan.value(10, 0)?;
    ensure!(scan.pos == bytes.len(), "trailing bytes after NBT root");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastnbt::{Value, nbt};
    use std::io::Write;

    fn fixture(version: i32, compression: u8) -> (tempfile::TempDir, Vec<u8>) {
        let mut blocks = vec![0i8; 4096];
        blocks[15] = 35;
        blocks[14] = 1;
        let mut metadata = vec![0i8; 2048];
        metadata[7] = 0xa3u8 as i8;
        let mut add = vec![0i8; 2048];
        add[7] = 0x20;
        let root = nbt!({
            "DataVersion": version,
            "Level": {
                "xPos": -1i32,
                "zPos": -1i32,
                "LastUpdate": 123456i64,
                "Biomes": ByteArray::new(vec![37; 256]),
                "Sections": [ {
                    "Y": 0i8,
                    "Blocks": ByteArray::new(blocks),
                    "Data": ByteArray::new(metadata),
                    "Add": ByteArray::new(add),
                    "BlockLight": ByteArray::new(vec![0x54; 2048]),
                    "SkyLight": ByteArray::new(vec![-1; 2048]),
                } ],
            },
        });
        let raw = fastnbt::to_bytes(&root).unwrap();
        let payload = if compression == 1 {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&raw).unwrap();
            encoder.finish().unwrap()
        } else {
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(&raw).unwrap();
            encoder.finish().unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let mut region = vec![0u8; 3 * 4096];
        region[1023 * 4..1023 * 4 + 4].copy_from_slice(&[0, 0, 2, 1]);
        region[8192..8196].copy_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
        region[8196] = compression;
        region[8197..8197 + payload.len()].copy_from_slice(&payload);
        std::fs::write(dir.path().join("r.-1.-1.mca"), region).unwrap();
        (dir, payload)
    }

    #[test]
    fn region_nibbles_negative_coordinates_and_fingerprints() {
        for compression in [1, 2] {
            let (dir, payload) = fixture(1343, compression);
            let world = World::open(dir.path(), 1024 * 1024).unwrap();
            assert_eq!(world.chunk_positions(), vec![(-1, -1)]);
            let odd = world.block(-1, 0, -16);
            assert_eq!(
                (odd.id, odd.data, odd.block_light, odd.sky_light, odd.biome),
                (547, 10, 5, 15, 37)
            );
            let even = world.block(-2, 0, -16);
            assert_eq!((even.id, even.data, even.block_light), (1, 3, 4));
            assert_eq!(world.block(-1, -1, -16).id, 0);
            assert_eq!(world.block(-1, 256, -16).id, 0);
            assert_eq!(world.block(0, 0, 0).id, 0);
            assert_eq!(world.block(-1, 16, -16).id, 0);
            assert_eq!(world.block(-1, 16, -16).sky_light, 15);
            let a = world.get_chunk(-1, -1).unwrap().unwrap();
            let b = world.get_chunk(-1, -1).unwrap().unwrap();
            assert_eq!(a.last_update, 123456);
            assert!(Arc::ptr_eq(&a, &b));
            let hashes = world.fingerprints().unwrap();
            assert_eq!(hashes["-1,-1"], payload_hash(compression, &payload));
            assert_eq!(hashes, world.fingerprints().unwrap());
        }
    }

    #[test]
    fn tolerant_allocations_skip_only_bad_chunks_without_changing_sources() {
        let (dir, _) = fixture(1343, 2);
        let path = dir.path().join("r.-1.-1.mca");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[..4].copy_from_slice(&[0, 0, 255, 1]);
        std::fs::write(&path, &bytes).unwrap();
        assert!(World::open(dir.path(), 0).unwrap().fingerprints().is_err());
        let world = World::open_with_options(dir.path(), 1024 * 1024, true).unwrap();
        assert_eq!(world.chunk_positions(), vec![(-1, -1)]);
        assert!(world.get_chunk(-1, -1).unwrap().is_some());
        assert!(world.get_chunk(-32, -32).unwrap().is_none());
        assert_eq!(world.fingerprints().unwrap().len(), 1);
        assert_eq!(world.skipped_chunks(), 1);
        world.check_consistent().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn tolerant_overlap_omits_both_owners_not_just_the_last() {
        let (dir, _) = fixture(1343, 2);
        let path = dir.path().join("r.-1.-1.mca");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[..4].copy_from_slice(&[0, 0, 2, 1]);
        std::fs::write(&path, &bytes).unwrap();
        let world = World::open_with_options(dir.path(), 0, true).unwrap();
        assert!(world.chunk_positions().is_empty());
        assert!(world.fingerprints().unwrap().is_empty());
        assert_eq!(world.skipped_chunks(), 2);
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn tolerant_payload_and_nbt_errors_are_cached_as_missing_with_no_writes() {
        for invalid_length in [false, true] {
            let (dir, _) = fixture(1343, 2);
            let path = dir.path().join("r.-1.-1.mca");
            let mut bytes = std::fs::read(&path).unwrap();
            if invalid_length {
                bytes[8192..8196].copy_from_slice(&u32::MAX.to_be_bytes());
            } else {
                bytes[8197..8201].fill(0xff);
            }
            std::fs::write(&path, &bytes).unwrap();
            let strict = World::open(dir.path(), 0).unwrap();
            assert!(strict.get_chunk(-1, -1).is_err());
            assert!(strict.check_consistent().is_err());
            let world = World::open_with_options(dir.path(), 0, true).unwrap();
            world.fingerprints().unwrap();
            for _ in 0..2 {
                assert!(world.get_chunk(-1, -1).unwrap().is_none());
            }
            assert_eq!(world.skipped_chunks(), 1);
            world.check_consistent().unwrap();
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn tolerant_truncated_header_still_detects_source_changes() {
        let (dir, _) = fixture(1343, 2);
        let path = dir.path().join("r.-1.-1.mca");
        std::fs::write(&path, [0; 128]).unwrap();
        assert!(World::open(dir.path(), 0).is_err());
        let world = World::open_with_options(dir.path(), 0, true).unwrap();
        assert!(world.fingerprints().unwrap().is_empty());
        std::fs::write(&path, [0; 256]).unwrap();
        assert!(world.check_consistent().is_err());
    }

    #[test]
    fn tolerant_mode_never_hides_missing_files_or_unsupported_versions() {
        let (dir, _) = fixture(1344, 2);
        let world = World::open_with_options(dir.path(), 0, true).unwrap();
        assert!(world.get_chunk(-1, -1).is_err());
        assert_eq!(world.skipped_chunks(), 0);
        assert!(world.check_consistent().is_err());
        let (dir, _) = fixture(1343, 2);
        let world = World::open_with_options(dir.path(), 0, true).unwrap();
        std::fs::remove_file(dir.path().join("r.-1.-1.mca")).unwrap();
        assert!(world.get_chunk(-1, -1).is_err());
        assert_eq!(world.skipped_chunks(), 0);
        assert!(world.check_consistent().is_err());
    }

    #[test]
    fn optional_arrays_and_old_version() {
        let root = nbt!({ "Level": { "xPos": 0i32, "zPos": 0i32,
            "Sections": [{ "Y": 0i8, "Blocks": ByteArray::new(vec![1;4096]) }] } });
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder
            .write_all(&fastnbt::to_bytes(&root).unwrap())
            .unwrap();
        let chunk = decode_chunk(2, &encoder.finish().unwrap(), 0, 0).unwrap();
        let block = chunk.block(0, 0, 0);
        assert_eq!(chunk.last_update, 0);
        assert_eq!(chunk.highest_block_y(0, 0), Some(15));
        assert!(chunk.column_section_present(0, 0, 0));
        assert!(!chunk.column_section_present(0, 16, 0));
        assert_eq!(
            (block.id, block.data, block.sky_light, block.biome),
            (1, 0, 0, 0)
        );
    }

    #[test]
    fn render_fingerprint_ignores_save_metadata_but_tracks_all_sampled_arrays() {
        let (_dir, payload) = fixture(1343, 2);
        let mut chunk = decode_chunk(2, &payload, -1, -1).unwrap();
        let original = chunk.render_fingerprint();
        chunk.last_update += 1;
        assert_eq!(chunk.render_fingerprint(), original);
        assert_eq!(chunk.highest_block_y(15, 0), Some(0));
        assert_eq!(chunk.highest_block_y(0, 0), None);
        assert_eq!(chunk.highest_block_y(16, 0), None);
        for which in 0..6 {
            let mut chunk = decode_chunk(2, &payload, -1, -1).unwrap();
            let section = chunk.sections[0].as_mut().unwrap();
            match which {
                0 => section.blocks[0] ^= 1,
                1 => section.add.as_mut().unwrap()[0] ^= 1,
                2 => section.data.as_mut().unwrap()[0] ^= 1,
                3 => section.block_light.as_mut().unwrap()[0] ^= 1,
                4 => section.sky_light.as_mut().unwrap()[0] ^= 1,
                _ => chunk.biomes[0] ^= 1,
            }
            assert_ne!(chunk.render_fingerprint(), original, "input {which}");
        }
    }

    #[test]
    fn render_fingerprint_ignores_encoded_ticks_and_entities() {
        let fingerprint = |tick: i64| {
            let root = nbt!({"Level":{"xPos":0i32,"zPos":0i32,"LastUpdate":tick,
                "InhabitedTime":tick,"Entities":[{"id":"Pig","Age":tick}],
                "Sections":[{"Y":0i8,"Blocks":ByteArray::new(vec![1;4096])}]}});
            let mut encoder =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder
                .write_all(&fastnbt::to_bytes(&root).unwrap())
                .unwrap();
            let payload = encoder.finish().unwrap();
            let chunk = decode_chunk(2, &payload, 0, 0).unwrap();
            (payload, chunk.render_fingerprint())
        };
        let (old_payload, old_hash) = fingerprint(1);
        let (new_payload, new_hash) = fingerprint(2);
        assert_ne!(payload_hash(2, &old_payload), payload_hash(2, &new_payload));
        assert_eq!(old_hash, new_hash);
    }

    #[test]
    fn occupancy_includes_extended_ids_and_top_height_and_retains_air_light() {
        let mut add = vec![0i8; 2048];
        add[4095 / 2] = 0x10;
        let root = nbt!({"Level":{"xPos":0i32,"zPos":0i32,
            "HeightMap": vec![0i32;256],
            "Sections":[{"Y":15i8,"Blocks":ByteArray::new(vec![0;4096]),
                "Add":ByteArray::new(add), "Data":ByteArray::new(vec![0;2048]),
                "SkyLight":ByteArray::new(vec![0x55;2048])}]}});
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder
            .write_all(&fastnbt::to_bytes(&root).unwrap())
            .unwrap();
        let chunk = decode_chunk(2, &encoder.finish().unwrap(), 0, 0).unwrap();
        assert_eq!(chunk.highest_block_y(15, 15), Some(255));
        assert_eq!(chunk.highest_block_y(0, 0), None);
        assert!(chunk.column_section_present(15, 255, 15));
        assert!(!chunk.column_section_present(0, 255, 0));
        assert_eq!(chunk.block(15, 255, 15).id, 256);
        assert_eq!(chunk.block(0, 255, 0).sky_light, 5);
        assert_eq!(chunk.block(0, 239, 0).sky_light, 15);
        assert!(chunk.sections[15].as_ref().unwrap().data.is_none());
    }

    #[test]
    fn rejects_newer_version_and_corrupt_lengths() {
        let (dir, _) = fixture(1344, 2);
        let world = World::open(dir.path(), 0).unwrap();
        assert!(world.get_chunk(-1, -1).is_err());
        assert_eq!(world.block(-1, 0, -16).id, 0);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("r.-1.-1.mca"))
            .unwrap();
        file.write_all_at(&u32::MAX.to_be_bytes(), 8192).unwrap();
        let world = World::open(dir.path(), 0).unwrap();
        assert!(world.get_chunk(-1, -1).is_err());
        assert!(world.fingerprints().is_err());
    }

    #[test]
    fn rejects_bad_sectors_and_truncated_headers() {
        let (dir, _) = fixture(1343, 2);
        let path = dir.path().join("r.-1.-1.mca");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[1023 * 4..1023 * 4 + 4].copy_from_slice(&[0, 0, 1, 1]);
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            World::open(dir.path(), 0)
                .unwrap()
                .get_chunk(-1, -1)
                .is_err()
        );
        std::fs::write(&path, [0; 42]).unwrap();
        assert!(World::open(dir.path(), 0).is_err());
    }

    #[test]
    fn limits_nbt_depth_and_decompression() {
        let mut value = Value::Int(1);
        for _ in 0..70 {
            value = nbt!({ "child": value });
        }
        assert!(validate_nbt(&fastnbt::to_bytes(&value).unwrap()).is_err());
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&vec![0; MAX_NBT + 1]).unwrap();
        assert!(
            decode_chunk(2, &encoder.finish().unwrap(), 0, 0)
                .err()
                .unwrap()
                .to_string()
                .contains("16 MiB")
        );
    }

    #[test]
    fn world_is_send_sync_and_cache_respects_budget() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<World>();
        let (dir, _) = fixture(1343, 2);
        let world = World::open(dir.path(), 1).unwrap();
        world.get_chunk(-1, -1).unwrap();
        assert_eq!(world.cache.lock().unwrap().bytes, 0);
        let fingerprints = world.decoded_render_fingerprints();
        assert_eq!(fingerprints.len(), 1);
        assert_eq!(
            fingerprints["-1,-1"],
            world
                .get_chunk(-1, -1)
                .unwrap()
                .unwrap()
                .render_fingerprint()
        );
        assert_eq!(world.cache.lock().unwrap().bytes, 0);
        assert!(world.get_chunk(0, 0).unwrap().is_none());
        assert_eq!(world.decoded_render_fingerprints(), fingerprints);
    }

    #[test]
    fn region_handles_are_not_retained() {
        let dir = tempfile::tempdir().unwrap();
        for x in 0..256 {
            std::fs::write(dir.path().join(format!("r.{x}.0.mca")), [0; 8192]).unwrap();
        }
        let world = World::open(dir.path(), 1024).unwrap();
        assert_eq!(world.regions.len(), 256);
        world.fingerprints().unwrap();
        // Count only this fixture's descriptors, so parallel tests cannot
        // affect the assertion. Both indexing and hashing must close files.
        for entry in std::fs::read_dir("/proc/self/fd").unwrap() {
            if let Ok(target) = std::fs::read_link(entry.unwrap().path()) {
                assert!(
                    !target.starts_with(dir.path()),
                    "retained region FD: {}",
                    target.display()
                );
            }
        }
    }

    #[test]
    fn consistency_rejects_cached_decode_failures() {
        let (dir, _) = fixture(1344, 2);
        let world = World::open(dir.path(), 1024 * 1024).unwrap();
        world.check_consistent().unwrap();
        assert_eq!(world.block(-1, 0, -16).id, 0);
        let error = world.check_consistent().unwrap_err().to_string();
        assert!(error.contains("unreadable chunk -1,-1"));
        assert!(error.contains("DataVersion"));
        // Failures remain sticky even when there is no decoded cache entry.
        assert!(world.get_chunk(-1, -1).is_err());
    }

    #[test]
    fn consistency_rejects_new_removed_and_modified_regions() {
        let (dir, _) = fixture(1343, 2);
        let world = World::open(dir.path(), 1024 * 1024).unwrap();
        world.get_chunk(-1, -1).unwrap();
        world.check_consistent().unwrap();
        let added = dir.path().join("r.0.0.mca");
        std::fs::write(&added, [0; 8192]).unwrap();
        assert!(world.check_consistent().is_err());
        assert!(world.fingerprints().is_err());
        std::fs::remove_file(added).unwrap();
        world.check_consistent().unwrap();
        let path = dir.path().join("r.-1.-1.mca");
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&[1], 4096).unwrap();
        assert!(world.check_consistent().is_err());
        std::fs::remove_file(path).unwrap();
        assert!(world.check_consistent().is_err());
    }

    #[test]
    fn concurrent_misses_share_cached_chunk() {
        let (dir, _) = fixture(1343, 2);
        let world = World::open(dir.path(), 1024 * 1024).unwrap();
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        world.get_chunk(-1, -1).unwrap().unwrap()
                    })
                })
                .collect();
            let chunks: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect();
            assert!(chunks.iter().all(|chunk| Arc::ptr_eq(chunk, &chunks[0])));
            let cache = world.cache.lock().unwrap();
            assert_eq!(cache.chunks.len(), 1);
            assert_eq!(cache.lru.len(), 1);
            assert_eq!(cache.bytes, chunks[0].memory_bytes());
        });
        world.check_consistent().unwrap();
    }

    #[test]
    fn missing_region_directory_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(World::open(&dir.path().join("missing-region"), 0).is_err());
    }

    #[test]
    fn persistent_region_cache_reuses_hashes_and_verify_rehashes() {
        let (dir, payload) = fixture(1343, 2);
        let world = World::open(dir.path(), 0).unwrap();
        let original = world.fingerprints_cached(&BTreeMap::new(), false).unwrap();
        assert_eq!(
            original["r.-1.-1.mca"].chunks["-1,-1"],
            payload_hash(2, &payload)
        );
        let serialized = serde_json::to_vec(&original).unwrap();
        let mut previous: BTreeMap<String, RegionFingerprint> =
            serde_json::from_slice(&serialized).unwrap();
        previous
            .get_mut("r.-1.-1.mca")
            .unwrap()
            .chunks
            .insert("-1,-1".into(), "saved-hash".into());
        let reopened = World::open(dir.path(), 0).unwrap();
        assert_eq!(
            reopened.fingerprints_cached(&previous, false).unwrap()["r.-1.-1.mca"].chunks["-1,-1"],
            "saved-hash"
        );
        assert_eq!(
            reopened.fingerprints_cached(&previous, true).unwrap()["r.-1.-1.mca"].chunks,
            original["r.-1.-1.mca"].chunks
        );
        previous.get_mut("r.-1.-1.mca").unwrap().stamp.ctime_ns ^= 1;
        assert_eq!(
            reopened.fingerprints_cached(&previous, false).unwrap()["r.-1.-1.mca"].chunks,
            original["r.-1.-1.mca"].chunks
        );
    }

    #[test]
    fn cached_regions_do_not_read_payloads() {
        let (dir, _) = fixture(1343, 2);
        let mut world = World::open(dir.path(), 0).unwrap();
        let previous = world.fingerprints_cached(&BTreeMap::new(), false).unwrap();
        // Make any payload-read attempt fail without touching the file stamp.
        world.locations.get_mut(&(-1, -1)).unwrap().invalid = Some("test payload read trap".into());
        assert!(world.fingerprints_cached(&previous, false).is_ok());
        assert!(world.fingerprints_cached(&previous, true).is_err());
    }

    #[test]
    fn region_cache_detects_changes_and_atomic_replacement() {
        let (dir, _) = fixture(1343, 2);
        let path = dir.path().join("r.-1.-1.mca");
        let world = World::open(dir.path(), 0).unwrap();
        let previous = world.fingerprints_cached(&BTreeMap::new(), false).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(&[1], 4096).unwrap();
        assert!(world.fingerprints_cached(&previous, false).is_err());
        assert!(world.fingerprints_cached(&previous, true).is_err());
        let reopened = World::open(dir.path(), 0).unwrap();
        let updated = reopened.fingerprints_cached(&previous, false).unwrap();
        assert_ne!(updated["r.-1.-1.mca"].stamp, previous["r.-1.-1.mca"].stamp);
        let replacement = dir.path().join("replacement");
        std::fs::copy(&path, &replacement).unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert!(reopened.fingerprints_cached(&updated, false).is_err());
        let newest = World::open(dir.path(), 0).unwrap();
        let latest = newest.fingerprints_cached(&updated, false).unwrap();
        assert_ne!(
            latest["r.-1.-1.mca"].stamp.inode,
            updated["r.-1.-1.mca"].stamp.inode
        );
    }
}
