//! Pixel-space projection: topdown (16x,16z), isometric
//! (16(rx+rz),8(rz-rx)-16y). Rotation is counterclockwise in x/z.
//! Rasterization uses affine nearest-neighbor textures and straight-alpha output.
//! Coverage is deliberately not vanilla model parity: straight stairs only,
//! approximate connected fences/panes and small redstone models, no entity meshes,
//! ambient occlusion, or JSON blockstate interpretation. Biome colors use a
//! cached horizontal 3x3 average; entity-atlas chests remain cuboid approximations.
use crate::{
    assets::{Assets, Shape, shape, transparent},
    config::{MapConfig, Mode, View},
    world::{Block, Chunk, World},
};
use image::RgbaImage;
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, LazyLock},
};

fn rotate(x: f64, z: f64, r: u8) -> (f64, f64) {
    match r % 4 {
        0 => (x, z),
        1 => (-z, x),
        2 => (-x, -z),
        _ => (z, -x),
    }
}
pub fn project(x: f64, y: f64, z: f64, view: View, rotation: u8) -> (f64, f64) {
    if view == View::Topdown {
        return (x * 16., z * 16.);
    }
    let (x, z) = rotate(x, z, rotation);
    ((x + z) * 16., (z - x) * 8. - y * 16.)
}
#[cfg(test)]
fn inverse(sx: f64, sy: f64, y: f64, view: View, r: u8) -> (f64, f64) {
    if view == View::Topdown {
        return (sx / 16., sy / 16.);
    }
    let sum = sx / 16.;
    let diff = (sy + y * 16.) / 8.;
    rotate((sum - diff) / 2., (sum + diff) / 2., (4 - r % 4) % 4)
}
/// Includes a two-block halo for faces, biome samples and lighting dependencies.
pub fn affected_tiles(
    chunk_x: i32,
    chunk_z: i32,
    view: View,
    rotation: u8,
    tile_size: u32,
) -> Vec<(i32, i32)> {
    assert!(tile_size > 0);
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for x in [
        f64::from(chunk_x) * 16. - 2.,
        f64::from(chunk_x) * 16. + 18.,
    ] {
        for z in [
            f64::from(chunk_z) * 16. - 2.,
            f64::from(chunk_z) * 16. + 18.,
        ] {
            for y in [0., 256.] {
                let (sx, sy) = project(x, y, z, view, rotation);
                bounds[0] = bounds[0].min(sx);
                bounds[1] = bounds[1].min(sy);
                bounds[2] = bounds[2].max(sx);
                bounds[3] = bounds[3].max(sy);
            }
        }
    }
    let t = f64::from(tile_size);
    let mut result = Vec::new();
    for ty in (bounds[1] / t).floor() as i32..=(bounds[3] / t).floor() as i32 {
        for tx in (bounds[0] / t).floor() as i32..=(bounds[2] / t).floor() as i32 {
            result.push((tx, ty));
        }
    }
    result
}

fn excluded(b: Block, exclusions: &[(u16, Option<u8>)]) -> bool {
    exclusions
        .iter()
        .any(|&(id, d)| id == b.id && d.is_none_or(|d| d == b.data))
}
fn solid(b: Block, exclusions: &[(u16, Option<u8>)]) -> bool {
    !excluded(b, exclusions) && !transparent(b.id) && shape(b.id) == Shape::Cube
}
fn joined(b: Block, n: Block, exclusions: &[(u16, Option<u8>)]) -> bool {
    solid(n, exclusions)
        || (!excluded(n, exclusions)
            && b.id == n.id
            && matches!(b.id, 8..=11 | 20 | 79 | 95 | 165 | 174 | 212))
}
// Each worker holds a 16x16 direct-mapped window of chunk Arcs for its current
// tile, in addition to the world's global cache budget. A maximum-size legacy
// chunk is about 192 KiB, so this is approximately 48 MiB of additional pinned
// chunk storage per worker, never a world-sized cache. A tile's chunk footprint
// is a narrow diagonal strip, so colliding entries are reloaded from the world
// cache rather than redecoded, and produce identical pixels either way.
// Missing chunks occupy slots too.
const TILE_CHUNK_SLOTS: usize = 256;
struct CachedChunk {
    // `None` until the slot is first filled; chunk coordinates, not block ones.
    position: Option<(i32, i32)>,
    chunk: Option<Arc<Chunk>>,
    tints: [Option<[[u8; 3]; 3]>; 256],
}
struct Sample<'a> {
    world: &'a World,
    slots: Box<[CachedChunk]>,
    /// Slot of the most recent lookup, revalidated by its coordinates.
    hot: usize,
    hot_position: Option<(i32, i32)>,
    failed: bool,
}
const AIR: Block = Block {
    id: 0,
    data: 0,
    block_light: 0,
    sky_light: 15,
    biome: 1,
};
/// Chunk and in-chunk coordinates. The divisor is a power of two, so an
/// arithmetic shift and a mask reproduce `div_euclid`/`rem_euclid` exactly,
/// including for negative coordinates, without the sign-correcting branch.
#[inline]
fn split(v: i32) -> (i32, usize) {
    (v >> 4, (v & 15) as usize)
}
impl<'a> Sample<'a> {
    fn new(world: &'a World) -> Self {
        Self {
            world,
            slots: (0..TILE_CHUNK_SLOTS)
                .map(|_| CachedChunk {
                    position: None,
                    chunk: None,
                    tints: [None; 256],
                })
                .collect(),
            hot: 0,
            hot_position: None,
            failed: false,
        }
    }
    /// Resolve block coordinates to a filled slot index.
    #[inline]
    fn slot(&mut self, x: i32, z: i32) -> usize {
        let position = (x >> 4, z >> 4);
        if self.hot_position == Some(position) {
            return self.hot;
        }
        let index = ((position.0 & 15) * 16 + (position.1 & 15)) as usize;
        if self.slots[index].position != Some(position) {
            let chunk = match self.world.get_chunk(position.0, position.1) {
                Ok(c) => c,
                Err(e) => {
                    if !self.failed {
                        tracing::warn!(error=%e,"chunk unavailable while rendering tile");
                        self.failed = true;
                    }
                    None
                }
            };
            self.slots[index] = CachedChunk {
                position: Some(position),
                chunk,
                tints: [None; 256],
            };
        }
        self.hot = index;
        self.hot_position = Some(position);
        index
    }
    fn chunk(&mut self, x: i32, z: i32) -> &mut CachedChunk {
        let index = self.slot(x, z);
        &mut self.slots[index]
    }
    fn block(&mut self, x: i32, y: i32, z: i32) -> Block {
        if !(0..256).contains(&y) {
            return AIR;
        }
        let index = self.slot(x, z);
        self.slots[index]
            .chunk
            .as_ref()
            .map_or(AIR, |c| c.block((x & 15) as u8, y as u8, (z & 15) as u8))
    }
    fn tint(&mut self, assets: &Assets, kind: u8, x: i32, z: i32) -> [u8; 3] {
        if kind == 0 {
            return [255; 3];
        }
        let index = split(z).1 * 16 + split(x).1;
        let channel = match kind {
            2 => 1,
            4 => 2,
            _ => 0,
        };
        if let Some(tints) = self.chunk(x, z).tints[index] {
            return tints[channel];
        }
        let mut sums = [[0u16; 3]; 3];
        for dz in -1..=1 {
            for dx in -1..=1 {
                let biome = self.block(x + dx, 0, z + dz).biome;
                for (sum, kind) in sums.iter_mut().zip([1, 2, 4]) {
                    for (sum, color) in sum.iter_mut().zip(assets.tint(kind, biome)) {
                        *sum += u16::from(color);
                    }
                }
            }
        }
        let tints = sums.map(|rgb| rgb.map(|c| (c / 9) as u8));
        self.chunk(x, z).tints[index] = Some(tints);
        tints[channel]
    }
}
struct Draw {
    x: i32,
    y: i32,
    z: i32,
    /// Camera depth rz-rx+y, set by the isometric sweep; unused for topdown.
    depth: i64,
    b: Block,
    tint: [u8; 3],
    connections: u8,
    faces: [bool; 3],
    light: [f64; 3],
}

const NEIGHBORS: [(i32, i32); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];
static LIGHT_LEVELS: LazyLock<[f64; 16]> = LazyLock::new(|| {
    std::array::from_fn(|level| {
        // The old rasterizer evaluated powi with a runtime exponent. LLVM's
        // constant-folded powi can differ by one ULP in release/LTO builds,
        // which can cross an integer color boundary. Pay the original runtime
        // calculation just 16 times; do not change rounding to gain speed.
        let level = std::hint::black_box(level as i32);
        0.16 + 0.84 * 0.8_f64.powi(15 - level)
    })
});
fn connects(b: Block, n: Block, dir: usize, exclusions: &[(u16, Option<u8>)]) -> bool {
    if excluded(n, exclusions) {
        return false;
    }
    if matches!(b.id, 101 | 102 | 160) {
        return solid(n, exclusions) || matches!(n.id, 20 | 95 | 101 | 102 | 160);
    }
    if matches!(n.id, 107 | 183..=187) {
        // Gate plane is east/west for north/south-facing metadata.
        return (n.data & 1 == 0) == (dir < 2);
    }
    if b.id == 139 {
        return n.id == 139 || solid(n, exclusions);
    }
    let same_fence = if b.id == 113 {
        n.id == 113
    } else {
        matches!(n.id, 85 | 188..=192)
    };
    same_fence || solid(n, exclusions)
}

fn draw_block(
    sample: &mut Sample<'_>,
    assets: &Assets,
    map: &MapConfig,
    exclusions: &[(u16, Option<u8>)],
    position: [i32; 3],
    b: Block,
    front: [(i32, i32); 2],
) -> Option<Draw> {
    let [x, y, z] = position;
    if b.id == 0 || shape(b.id) == Shape::Air || excluded(b, exclusions) {
        return None;
    }
    let top = sample.block(x, y + 1, z);
    let a = if map.view == View::Isometric {
        sample.block(x + front[0].0, y, z + front[0].1)
    } else {
        top
    };
    let c = if map.view == View::Isometric {
        sample.block(x + front[1].0, y, z + front[1].1)
    } else {
        top
    };
    let full = shape(b.id) == Shape::Cube && !matches!(b.id, 54 | 60 | 81 | 130 | 146 | 208);
    let faces = [
        !joined(b, top, exclusions) || !full,
        map.view == View::Isometric && (!joined(b, a, exclusions) || !full),
        map.view == View::Isometric && (!joined(b, c, exclusions) || !full),
    ];
    if !faces.iter().any(|&f| f) {
        return None;
    }
    let m = assets.material(b.id, b.data);
    let light = [top, a, c].map(|n| {
        if map.mode == Mode::Plain {
            1.
        } else {
            let level = n.sky_light.max(n.block_light).max(b.block_light).min(15);
            let intensity = LIGHT_LEVELS[usize::from(level)];
            if matches!(b.id, 8 | 9) {
                1. - (1. - intensity) * f64::from(map.lighting_water_intensity)
            } else {
                intensity
            }
        }
    });
    let tint = sample.tint(assets, m.tint, x, z);
    let mut connections = 0;
    if m.shape == Shape::Fence {
        for (dir, (dx, dz)) in NEIGHBORS.into_iter().enumerate() {
            if connects(b, sample.block(x + dx, y, z + dz), dir, exclusions) {
                connections |= 1 << dir;
            }
        }
    }
    Some(Draw {
        x,
        y,
        z,
        depth: 0,
        b,
        tint,
        connections,
        faces,
        light,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep the public tile renderer API explicit and compatible with output workers"
)]
pub fn render_tile(
    world: &World,
    assets: &Assets,
    map: &MapConfig,
    rotation: u8,
    exclusions: &[(u16, Option<u8>)],
    tile_x: i32,
    tile_y: i32,
    tile_size: u32,
) -> RgbaImage {
    let mut image = RgbaImage::new(tile_size, tile_size);
    if tile_size == 0 {
        return image;
    }
    let ox = f64::from(tile_x) * f64::from(tile_size);
    let oy = f64::from(tile_y) * f64::from(tile_size);
    let mut sample = Sample::new(world);
    let mut draws = Vec::new();
    let (nx, nz) = rotate(-1., 0., (4 - rotation % 4) % 4);
    let front_x = (nx as i32, nz as i32);
    let (nx, nz) = rotate(0., 1., (4 - rotation % 4) % 4);
    let front_z = (nx as i32, nz as i32);
    if map.view == View::Topdown {
        // One chunk lookup per column, then direct column reads. A column-local
        // stack replaces the tile-wide covered HashSet and draw sort. Its hard
        // maximum is 256 records, including fully transparent vertical stacks.
        let mut painter = Painter {
            image: &mut image,
            assets,
            map,
            rotation,
            ox,
            oy,
            shades: ShadeCache::default(),
            stamps: StampCache::default(),
            sprites: SpriteCache::default(),
            alphas: AlphaCache::default(),
        };
        for z in (oy / 16.).floor() as i32..((oy + f64::from(tile_size)) / 16.).ceil() as i32 {
            for x in (ox / 16.).floor() as i32..((ox + f64::from(tile_size)) / 16.).ceil() as i32 {
                let Some(chunk) = sample.chunk(x, z).chunk.clone() else {
                    continue;
                };
                draws.clear();
                let Some(highest) =
                    chunk.highest_block_y(x.rem_euclid(16) as u8, z.rem_euclid(16) as u8)
                else {
                    continue;
                };
                for y in (0..=i32::from(highest)).rev() {
                    let b = chunk.block(x.rem_euclid(16) as u8, y as u8, z.rem_euclid(16) as u8);
                    if let Some(d) = draw_block(
                        &mut sample,
                        assets,
                        map,
                        exclusions,
                        [x, y, z],
                        b,
                        [front_x, front_z],
                    ) {
                        draws.push(d);
                    }
                    if solid(b, exclusions) {
                        break;
                    }
                }
                for d in draws.iter().rev() {
                    painter.draw(d);
                }
            }
        }
        return image;
    }
    let mut painter = Painter {
        image: &mut image,
        assets,
        map,
        rotation,
        ox,
        oy,
        shades: ShadeCache::default(),
        stamps: StampCache::default(),
        sprites: SpriteCache::default(),
        alphas: AlphaCache::default(),
    };
    // Walk columns, not camera-depth planes. With rotated coordinates,
    // sum=rx+rz and diff=rz-rx select a column; block centers satisfy
    // sx=16(sum+1) and sy=8*diff-16y-8, so a column contributes only the
    // (tile_size+66)/16 elevations whose centers can reach this tile. Clamping
    // that window to the column's real top skips the empty sky that dominates
    // a plane-major sweep, and resolves the chunk and column once per column
    // instead of once per probed elevation.
    let sum_min = ((ox - 33.) / 16. - 1.).ceil() as i64;
    let sum_max = ((ox + f64::from(tile_size) + 33.) / 16. - 1.).floor() as i64;
    // Derived from the plane bounds: depth=diff+y with 0<=y<=255 covers exactly
    // the same (column, elevation) pairs the plane sweep enumerated.
    let diff_min = ((oy - 25.) / 8.).ceil() as i64;
    let diff_max = ((oy + f64::from(tile_size) + 4121.) / 8.).floor() as i64;
    for diff in diff_min..=diff_max {
        let ymin = ((8. * diff as f64 - 41. - oy - f64::from(tile_size)) / 16.)
            .ceil()
            .max(0.) as i32;
        let ymax = ((8. * diff as f64 + 25. - oy) / 16.).floor().min(255.) as i32;
        if ymin > ymax {
            continue;
        }
        // rx=(sum-diff)/2 is only integral when sum and diff share parity.
        let first = sum_min + ((sum_min - diff) & 1);
        for sum in (first..=sum_max).step_by(2) {
            let (rx, rz) = ((sum - diff) / 2, (sum + diff) / 2);
            let (x, z) = match rotation % 4 {
                0 => (rx, rz),
                1 => (rz, -rx),
                2 => (-rx, -rz),
                _ => (-rz, rx),
            };
            let (Ok(x), Ok(z)) = (i32::try_from(x), i32::try_from(z)) else {
                continue;
            };
            let (lx, lz) = ((x & 15) as u8, (z & 15) as u8);
            let slot = sample.slot(x, z);
            let Some(chunk) = sample.slots[slot].chunk.as_ref() else {
                continue;
            };
            let Some(top) = chunk.highest_block_y(lx, lz) else {
                continue;
            };
            let ymax = ymax.min(i32::from(top));
            if ymin > ymax {
                continue;
            }
            let chunk = chunk.clone();
            for y in ymin..=ymax {
                // Occupancy is only a draw-candidate shortcut; neighbor reads in
                // draw_block still go through `Sample::block`.
                if !chunk.column_section_present(lx, y as u8, lz) {
                    continue;
                }
                let b = chunk.block(lx, y as u8, lz);
                if let Some(mut d) = draw_block(
                    &mut sample,
                    assets,
                    map,
                    exclusions,
                    [x, y, z],
                    b,
                    [front_x, front_z],
                ) {
                    d.depth = diff + i64::from(y);
                    draws.push(d);
                }
            }
        }
    }
    // Back-to-front order. Within one camera-depth plane (y, z, x) is unique,
    // so this is the same total order the plane-major sweep produced.
    draws.sort_unstable_by_key(|d| (d.depth, d.y, d.z, d.x));
    for d in &draws {
        painter.draw(d);
    }
    image
}

// Bounded per-tile exact color lookup tables. The expression and operation order
// match the reference rasterizer; only repeated floating-point work is removed.
type ShadeKey = ([u8; 3], u64);
/// Block geometry projects onto an exact half-pixel lattice: every model
/// coordinate is a multiple of 1/16, and both projections scale by 8 or 16.
/// A quad's covered pixels and sampled texels therefore depend only on its
/// shape and sub-pixel origin, never on where in the tile it lands, so the
/// inverse UV solve can be done once per shape instead of once per pixel.
/// Keys hold exact f64 bits, so a cache hit reproduces the original math
/// bit-for-bit; all arithmetic stays on exactly representable multiples of 0.5.
type StampKey = ([u64; 4], u64, u64, u8);
struct Stamp {
    /// Pixel offsets from the quad's integer origin, with the texel each samples.
    texels: Vec<(i16, i16, u8)>,
    width: i32,
    height: i32,
}
#[derive(Default)]
struct StampCache {
    stamps: Vec<(StampKey, Stamp)>,
    hot: usize,
}
impl StampCache {
    /// Quads arrive in a short repeating cycle of face shapes, so a hot slot
    /// plus a linear scan of integer keys beats hashing a 49-byte key for a
    /// quad that only covers a few hundred pixels.
    fn get(&mut self, key: StampKey, build: impl FnOnce() -> Stamp) -> &Stamp {
        if self.stamps.get(self.hot).is_none_or(|(k, _)| *k != key) {
            match self.stamps.iter().position(|(k, _)| *k == key) {
                Some(found) => self.hot = found,
                None => {
                    // Distinct shapes per view are bounded by the model set,
                    // not by the world. Reset rather than grow without limit.
                    if self.stamps.len() == 256 {
                        self.stamps.clear();
                    }
                    self.hot = self.stamps.len();
                    self.stamps.push((key, build()));
                }
            }
        }
        &self.stamps[self.hot].1
    }
}
/// A stamp plus the texture, tint, light and opacity it is drawn with fully
/// determines the composited pixels. Terrain repeats those combinations
/// constantly, so caching the finished sprite removes the texel indirection,
/// the alpha test and the three shade lookups from the inner loop, and lets
/// fully opaque spans be copied rather than blended.
type SpriteKey = (StampKey, u32, [u8; 3], u64, u64);
#[derive(Default)]
struct Sprite {
    /// Horizontal runs of fully opaque pixels: local x, local y, colors range.
    opaque: Vec<(i16, i16, u32, u32)>,
    colors: Vec<[u8; 4]>,
    /// Pixels that still need source-over compositing.
    blended: Vec<(i16, i16, [u8; 4])>,
    width: i32,
    height: i32,
}
struct SpriteCache {
    slots: Box<[Option<(SpriteKey, Sprite)>]>,
    hot: usize,
}
const SPRITE_SLOTS: usize = 2048;
impl Default for SpriteCache {
    fn default() -> Self {
        Self {
            slots: (0..SPRITE_SLOTS).map(|_| None).collect(),
            hot: 0,
        }
    }
}
impl SpriteCache {
    /// Direct-mapped: a collision rebuilds the sprite, which is cheap and
    /// produces identical pixels, so the cache needs no eviction bookkeeping.
    fn get(&mut self, key: SpriteKey, build: impl FnOnce() -> Sprite) -> &Sprite {
        if self.slots[self.hot].as_ref().is_none_or(|(k, _)| *k != key) {
            let mut hash = 0xcbf2_9ce4_8422_2325u64;
            for word in [
                key.0.0[0],
                key.0.0[1],
                key.0.0[2],
                key.0.0[3],
                key.0.1,
                key.0.2,
                u64::from(key.0.3),
                u64::from(key.1),
                u64::from(key.2[0]) << 16 | u64::from(key.2[1]) << 8 | u64::from(key.2[2]),
                key.3,
                key.4,
            ] {
                hash = (hash ^ word).wrapping_mul(0x1000_0000_01b3);
            }
            let index = (hash >> 17) as usize % SPRITE_SLOTS;
            if self.slots[index].as_ref().is_none_or(|(k, _)| *k != key) {
                self.slots[index] = Some((key, build()));
            }
            self.hot = index;
        }
        &self.slots[self.hot].as_ref().expect("slot just filled").1
    }
}
/// Straight-alpha opacity scaling of a texture's alpha channel. Opacity is a
/// per-map constant, so one table serves every quad drawn with it.
struct AlphaCache {
    bits: Option<u64>,
    table: [u8; 256],
}
impl Default for AlphaCache {
    fn default() -> Self {
        Self {
            bits: None,
            table: [0; 256],
        }
    }
}
impl AlphaCache {
    fn table(&mut self, opacity: f64) -> &[u8; 256] {
        if self.bits != Some(opacity.to_bits()) {
            self.table =
                std::array::from_fn(|a| (a as f64 * opacity).round().clamp(0., 255.) as u8);
            self.bits = Some(opacity.to_bits());
        }
        &self.table
    }
}
#[derive(Default)]
struct ShadeCache {
    tables: HashMap<ShadeKey, Box<[[u8; 256]; 3]>>,
    order: VecDeque<ShadeKey>,
}
impl ShadeCache {
    fn table(&mut self, tint: [u8; 3], light: f64) -> &[[u8; 256]; 3] {
        let key = (tint, light.to_bits());
        if !self.tables.contains_key(&key) {
            if self.tables.len() == 128 {
                self.tables.remove(&self.order.pop_front().unwrap());
            }
            let table = std::array::from_fn(|i| {
                std::array::from_fn(|c| {
                    (c as f64 * f64::from(tint[i]) / 255. * light).clamp(0., 255.) as u8
                })
            });
            self.tables.insert(key, Box::new(table));
            self.order.push_back(key);
        }
        &self.tables[&key]
    }
}

struct Painter<'a> {
    image: &'a mut RgbaImage,
    assets: &'a Assets,
    map: &'a MapConfig,
    rotation: u8,
    ox: f64,
    oy: f64,
    shades: ShadeCache,
    stamps: StampCache,
    sprites: SpriteCache,
    alphas: AlphaCache,
}
impl Painter<'_> {
    fn point(&self, p: [f64; 3]) -> (f64, f64) {
        let (x, y) = project(p[0], p[1], p[2], self.map.view, self.rotation);
        (x - self.ox, y - self.oy)
    }
    /// Affine parallelogram, half-open texel ownership avoids double-alpha seams.
    fn quad(&mut self, p: [[f64; 3]; 3], texture: usize, tint: [u8; 3], light: f64, opacity: f64) {
        self.quad_uv(p, texture, tint, light, opacity, 0);
    }
    fn quad_uv(
        &mut self,
        p: [[f64; 3]; 3],
        texture: usize,
        tint: [u8; 3],
        light: f64,
        opacity: f64,
        turns: u8,
    ) {
        let (ax, ay) = self.point(p[0]);
        let (bx, by) = self.point(p[1]);
        let (cx, cy) = self.point(p[2]);
        let ux = bx - ax;
        let uy = by - ay;
        let vx = cx - ax;
        let vy = cy - ay;
        let det = ux * vy - uy * vx;
        if det.abs() < 0.0001 {
            return;
        }
        let inv_det = 1. / det;
        let dx = bx + cx - ax;
        let dy = by + cy - ay;
        let width = self.image.width() as i32;
        let height = self.image.height() as i32;
        if ax.min(bx).min(cx).min(dx) >= f64::from(width)
            || ay.min(by).min(cy).min(dy) >= f64::from(height)
            || ax.max(bx).max(cx).max(dx) <= 0.
            || ay.max(by).max(cy).max(dy) <= 0.
        {
            return;
        }
        // Stamp-local pixel centers are the absolute ones minus an integer
        // origin. Every term is a multiple of 0.5 well inside f64's exact
        // integer range, so the subtraction is lossless and `u`/`v` match the
        // per-pixel solve bit-for-bit.
        let origin_x = ax.min(bx).min(cx).min(dx).floor();
        let origin_y = ay.min(by).min(cy).min(dy).floor();
        let fa = ax - origin_x;
        let fay = ay - origin_y;
        let key = (
            [ux.to_bits(), uy.to_bits(), vx.to_bits(), vy.to_bits()],
            fa.to_bits(),
            fay.to_bits(),
            turns & 3,
        );
        let stamp = self.stamps.get(key, || {
            let width = (ax.max(bx).max(cx).max(dx).ceil() - origin_x) as i32;
            let height = (ay.max(by).max(cy).max(dy).ceil() - origin_y) as i32;
            let mut texels = Vec::new();
            for ly in 0..height {
                for lx in 0..width {
                    let px = f64::from(lx) + 0.5 - fa;
                    let py = f64::from(ly) + 0.5 - fay;
                    let u = (px * vy - py * vx) * inv_det;
                    let v = (ux * py - uy * px) * inv_det;
                    if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
                        continue;
                    }
                    let (u, v) = match turns & 3 {
                        0 => (u, v),
                        1 => (v, 1. - u),
                        2 => (1. - u, 1. - v),
                        _ => (1. - v, u),
                    };
                    let tu = (u.clamp(0., 0.9999) * 16.) as u32;
                    let tv = (v.clamp(0., 0.9999) * 16.) as u32;
                    texels.push((lx as i16, ly as i16, (tv * 16 + tu) as u8));
                }
            }
            Stamp {
                texels,
                width,
                height,
            }
        });
        let shades = &mut self.shades;
        let alphas = &mut self.alphas;
        let assets = self.assets;
        let sprite = self.sprites.get(
            (
                key,
                texture as u32,
                tint,
                light.to_bits(),
                opacity.to_bits(),
            ),
            || {
                let shades = shades.table(tint, light);
                let alphas = (opacity != 1.).then(|| alphas.table(opacity));
                let source = assets.texels(texture);
                let mut sprite = Sprite {
                    width: stamp.width,
                    height: stamp.height,
                    ..Sprite::default()
                };
                for &(lx, ly, texel) in &stamp.texels {
                    let mut color = source[usize::from(texel)];
                    if let Some(alphas) = alphas {
                        color[3] = alphas[usize::from(color[3])];
                    }
                    if color[3] == 0 {
                        continue;
                    }
                    for i in 0..3 {
                        color[i] = shades[i][usize::from(color[i])];
                    }
                    if color[3] != 255 {
                        sprite.blended.push((lx, ly, color));
                        continue;
                    }
                    match sprite.opaque.last_mut() {
                        // Stamp pixels are emitted in scan order, so an opaque
                        // run simply extends while x stays contiguous.
                        Some(run)
                            if run.1 == ly && i32::from(run.0) + run.3 as i32 == i32::from(lx) =>
                        {
                            run.3 += 1;
                        }
                        _ => sprite.opaque.push((lx, ly, sprite.colors.len() as u32, 1)),
                    }
                    sprite.colors.push(color);
                }
                sprite
            },
        );
        let (pixels, rest) = self.image.as_chunks_mut::<4>();
        debug_assert!(rest.is_empty());
        let base_x = origin_x as i32;
        let base_y = origin_y as i32;
        let clipped = base_x < 0
            || base_y < 0
            || base_x + sprite.width > width
            || base_y + sprite.height > height;
        for &(lx, ly, start, len) in &sprite.opaque {
            let x = base_x + i32::from(lx);
            let y = base_y + i32::from(ly);
            let colors = &sprite.colors[start as usize..(start + len) as usize];
            if !clipped {
                let offset = (y * width + x) as usize;
                pixels[offset..offset + colors.len()].copy_from_slice(colors);
                continue;
            }
            if y < 0 || y >= height {
                continue;
            }
            let low = x.max(0);
            let high = (x + len as i32).min(width);
            for px in low..high {
                pixels[(y * width + px) as usize] = colors[(px - x) as usize];
            }
        }
        for &(lx, ly, color) in &sprite.blended {
            let x = base_x + i32::from(lx);
            let y = base_y + i32::from(ly);
            if clipped && (x < 0 || y < 0 || x >= width || y >= height) {
                continue;
            }
            blend(&mut pixels[(y * width + x) as usize], color);
        }
    }
    fn draw(&mut self, d: &Draw) {
        let (x, y, z) = (f64::from(d.x), f64::from(d.y), f64::from(d.z));
        let m = self.assets.material(d.b.id, d.b.data);
        let mut tint = d.tint;
        // Conifer/birch foliage has fixed coloration in the vanilla renderer.
        if d.b.id == 18 && d.b.data & 3 == 1 {
            tint = [97, 153, 97];
        }
        if d.b.id == 18 && d.b.data & 3 == 2 {
            tint = [128, 167, 85];
        }
        if d.b.id == 55 {
            let power = f64::from(d.b.data & 15) / 15.;
            tint = [
                (255. * if power == 0. { 0.3 } else { 0.4 + 0.6 * power }) as u8,
                (255. * (power * power * 0.7 - 0.5).max(0.)) as u8,
                0,
            ];
        }
        let opacity = if matches!(d.b.id, 8 | 9) {
            f64::from(self.map.water_opacity)
        } else {
            1.
        };
        if m.shape == Shape::Plant {
            if self.map.view == View::Topdown {
                self.quad(
                    [[x, y + 0.9, z], [x + 1., y + 0.9, z], [x, y + 0.9, z + 1.]],
                    m.top,
                    tint,
                    d.light[0],
                    opacity,
                );
            } else {
                self.quad(
                    [[x, y + 1., z], [x + 1., y + 1., z + 1.], [x, y, z]],
                    m.side,
                    tint,
                    d.light[0],
                    opacity,
                );
                self.quad(
                    [[x, y + 1., z + 1.], [x + 1., y + 1., z], [x, y, z + 1.]],
                    m.side,
                    tint,
                    d.light[0],
                    opacity,
                );
            }
            return;
        }
        let mut boxes = [[0., 0., 0., 1., 1., 1.]; 9];
        let mut count = 1;
        match m.shape {
            Shape::Slab => {
                if d.b.data & 8 == 0 {
                    boxes[0][4] = 0.5;
                } else {
                    boxes[0][1] = 0.5;
                }
            }
            Shape::Snow => boxes[0][4] = f64::from((d.b.data & 7) + 1) / 8.,
            Shape::Thin => {
                if d.b.id == 34 {
                    boxes[1] = [0.375, 0., 0.375, 0.625, 1., 0.625];
                    match d.b.data & 7 {
                        0 => {
                            boxes[0][4] = 0.25;
                            boxes[1][1] = 0.25;
                        }
                        1 => {
                            boxes[0][1] = 0.75;
                            boxes[1][4] = 0.75;
                        }
                        2 => {
                            boxes[0][5] = 0.25;
                            boxes[1] = [0.375, 0.375, 0.25, 0.625, 0.625, 1.];
                        }
                        3 => {
                            boxes[0][2] = 0.75;
                            boxes[1] = [0.375, 0.375, 0., 0.625, 0.625, 0.75];
                        }
                        4 => {
                            boxes[0][3] = 0.25;
                            boxes[1] = [0.25, 0.375, 0.375, 1., 0.625, 0.625];
                        }
                        _ => {
                            boxes[0][0] = 0.75;
                            boxes[1] = [0., 0.375, 0.375, 0.75, 0.625, 0.625];
                        }
                    }
                    count = 2;
                } else if matches!(d.b.id, 77 | 143) {
                    let thickness = if d.b.data & 8 != 0 { 0.0625 } else { 0.125 };
                    boxes[0] = [0.3125, 0.375, 0.3125, 0.6875, 0.625, 0.6875];
                    match d.b.data & 7 {
                        0 => {
                            boxes[0][1] = 1. - thickness;
                            boxes[0][4] = 1.;
                        }
                        1 => {
                            boxes[0][0] = 0.;
                            boxes[0][3] = thickness;
                        }
                        2 => {
                            boxes[0][0] = 1. - thickness;
                            boxes[0][3] = 1.;
                        }
                        3 => {
                            boxes[0][2] = 0.;
                            boxes[0][5] = thickness;
                        }
                        4 => {
                            boxes[0][2] = 1. - thickness;
                            boxes[0][5] = 1.;
                        }
                        _ => {
                            boxes[0][1] = 0.;
                            boxes[0][4] = thickness;
                        }
                    }
                } else if d.b.id == 69 {
                    boxes[0] = [0.3125, 0., 0.25, 0.6875, 0.1875, 0.75];
                    boxes[1] = [0.4375, 0.1875, 0.4375, 0.5625, 0.625, 0.5625];
                    match d.b.data & 7 {
                        0 | 7 => {
                            boxes[0][1] = 0.8125;
                            boxes[0][4] = 1.;
                            boxes[1][1] = 0.375;
                            boxes[1][4] = 0.8125;
                        }
                        1 => {
                            boxes[0] = [0., 0.25, 0.3125, 0.1875, 0.75, 0.6875];
                            boxes[1] = [0.1875, 0.4375, 0.4375, 0.625, 0.5625, 0.5625];
                        }
                        2 => {
                            boxes[0] = [0.8125, 0.25, 0.3125, 1., 0.75, 0.6875];
                            boxes[1] = [0.375, 0.4375, 0.4375, 0.8125, 0.5625, 0.5625];
                        }
                        3 => {
                            boxes[0] = [0.3125, 0.25, 0., 0.6875, 0.75, 0.1875];
                            boxes[1] = [0.4375, 0.4375, 0.1875, 0.5625, 0.5625, 0.625];
                        }
                        4 => {
                            boxes[0] = [0.3125, 0.25, 0.8125, 0.6875, 0.75, 1.];
                            boxes[1] = [0.4375, 0.4375, 0.375, 0.5625, 0.5625, 0.8125];
                        }
                        _ => {}
                    }
                    count = 2;
                } else if d.b.id == 131 {
                    boxes[0] = [0.375, 0.1875, 0.375, 0.625, 0.625, 0.625];
                    match d.b.data & 3 {
                        0 => {
                            boxes[0][2] = 0.;
                            boxes[0][5] = 0.375;
                        }
                        1 => {
                            boxes[0][0] = 0.625;
                            boxes[0][3] = 1.;
                        }
                        2 => {
                            boxes[0][2] = 0.625;
                            boxes[0][5] = 1.;
                        }
                        _ => {
                            boxes[0][0] = 0.;
                            boxes[0][3] = 0.375;
                        }
                    }
                } else if matches!(d.b.id, 96 | 167) {
                    if d.b.data & 4 != 0 {
                        match d.b.data & 3 {
                            0 => boxes[0][2] = 0.8125,
                            1 => boxes[0][5] = 0.1875,
                            2 => boxes[0][0] = 0.8125,
                            _ => boxes[0][3] = 0.1875,
                        }
                    } else if d.b.data & 8 == 0 {
                        boxes[0][4] = 0.1875;
                    } else {
                        boxes[0][1] = 0.8125;
                    }
                } else {
                    boxes[0][4] = if matches!(d.b.id, 93 | 94 | 149 | 150) {
                        0.125
                    } else if matches!(d.b.id, 55 | 132) {
                        0.015625
                    } else {
                        0.0625
                    };
                }
            }
            Shape::Fence => {
                let pane = matches!(d.b.id, 101 | 102 | 160);
                let (lo, hi) = if pane {
                    (0.4375, 0.5625)
                } else if d.b.id == 139 {
                    (0.25, 0.75)
                } else {
                    (0.375, 0.625)
                };
                boxes[0] = [lo, 0., lo, hi, 1., hi];
                for dir in 0..4 {
                    if d.connections & (1 << dir) == 0 {
                        continue;
                    }
                    let mut arm = [lo, 0., lo, hi, 1., hi];
                    match dir {
                        0 => {
                            arm[0] = 0.;
                            arm[3] = lo;
                        }
                        1 => {
                            arm[0] = hi;
                            arm[3] = 1.;
                        }
                        2 => {
                            arm[2] = 0.;
                            arm[5] = lo;
                        }
                        _ => {
                            arm[2] = hi;
                            arm[5] = 1.;
                        }
                    }
                    if pane || d.b.id == 139 {
                        boxes[count] = arm;
                        count += 1;
                    } else {
                        for (bottom, top) in [(0.375, 0.5625), (0.75, 0.9375)] {
                            let mut rail = arm;
                            rail[1] = bottom;
                            rail[4] = top;
                            if dir < 2 {
                                rail[2] = 0.4375;
                                rail[5] = 0.5625;
                            } else {
                                rail[0] = 0.4375;
                                rail[3] = 0.5625;
                            }
                            boxes[count] = rail;
                            count += 1;
                        }
                    }
                }
            }
            Shape::Stair => {
                let upside = d.b.data & 4 != 0;
                boxes[0] = [
                    0.,
                    if upside { 0.5 } else { 0. },
                    0.,
                    1.,
                    if upside { 1. } else { 0.5 },
                    1.,
                ];
                boxes[1] = [
                    0.,
                    if upside { 0. } else { 0.5 },
                    0.,
                    1.,
                    if upside { 0.5 } else { 1. },
                    1.,
                ];
                match d.b.data & 3 {
                    0 => boxes[1][0] = 0.5,
                    1 => boxes[1][3] = 0.5,
                    2 => boxes[1][2] = 0.5,
                    _ => boxes[1][5] = 0.5,
                }
                count = 2;
            }
            _ => {
                if matches!(d.b.id, 8..=11) {
                    boxes[0][4] = if d.faces[0] { 0.875 } else { 1. };
                }
                if d.b.id == 60 || d.b.id == 208 {
                    boxes[0][4] = 0.9375;
                }
                if d.b.id == 81 {
                    boxes[0] = [0.0625, 0., 0.0625, 0.9375, 1., 0.9375];
                }
                if matches!(d.b.id, 54 | 130 | 146) {
                    boxes[0] = [0.0625, 0., 0.0625, 0.9375, 0.875, 0.9375];
                }
            }
        }
        // Within stairs draw lower/upper pieces in camera depth order as well.
        boxes[..count].sort_by(|a, b| {
            let depth = |v: &[f64; 6]| {
                let (rx, rz) = rotate(v[0] + v[3], v[2] + v[5], self.rotation);
                rz - rx + v[1] + v[4]
            };
            depth(a).total_cmp(&depth(b))
        });
        for b in &boxes[..count] {
            let (x0, y0, z0, x1, y1, z1) =
                (x + b[0], y + b[1], z + b[2], x + b[3], y + b[4], z + b[5]);
            // Two camera-facing sides first; top last. For fractional shapes
            // internal faces are an approximation (no blockstate model mesh).
            if self.map.view == View::Isometric {
                let side_tint = if m.tint == 1 { [255; 3] } else { tint };
                for face in 0..2 {
                    if !d.faces[face + 1] {
                        continue;
                    }
                    let direction = if face == 0 { (-1., 0.) } else { (0., 1.) };
                    let (nx, nz) = rotate(direction.0, direction.1, (4 - self.rotation % 4) % 4);
                    let p = if nx < -0.5 {
                        [[x0, y1, z0], [x0, y1, z1], [x0, y0, z0]]
                    } else if nx > 0.5 {
                        [[x1, y1, z1], [x1, y1, z0], [x1, y0, z1]]
                    } else if nz < -0.5 {
                        [[x1, y1, z0], [x0, y1, z0], [x1, y0, z0]]
                    } else {
                        [[x0, y1, z1], [x1, y1, z1], [x0, y0, z1]]
                    };
                    let side = if nx < -0.5 {
                        0
                    } else if nx > 0.5 {
                        1
                    } else if nz < -0.5 {
                        2
                    } else {
                        3
                    };
                    self.quad(
                        p,
                        m.sides[side],
                        side_tint,
                        d.light[face + 1] * if face == 0 { 0.72 } else { 0.86 },
                        opacity,
                    );
                }
            }
            if d.faces[0] {
                self.quad_uv(
                    [[x0, y1, z0], [x1, y1, z0], [x0, y1, z1]],
                    m.top,
                    tint,
                    d.light[0],
                    opacity,
                    m.top_turns,
                );
            }
        }
    }
}

/// Source-over in straight RGBA; transparent background retains true alpha.
fn blend(dst: &mut [u8; 4], src: [u8; 4]) {
    let sa = u32::from(src[3]);
    if sa == 0 {
        return;
    }
    if sa == 255 {
        *dst = src;
        return;
    }
    let da = u32::from(dst[3]);
    let alpha = sa * 255 + da * (255 - sa);
    for i in 0..3 {
        dst[i] = ((u32::from(src[i]) * sa * 255 + u32::from(dst[i]) * da * (255 - sa) + alpha / 2)
            / alpha) as u8;
    }
    dst[3] = ((alpha + 127) / 255) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastnbt::{ByteArray, nbt};
    use std::io::Write;

    #[test]
    fn cached_shades_preserve_reference_rounding() {
        let mut shades = ShadeCache::default();
        for tint in [[255; 3], [97, 153, 97], [63, 118, 228], [0, 1, 254]] {
            for level in 0..16 {
                let intensity = 0.16 + 0.84 * 0.8_f64.powi(15 - std::hint::black_box(level));
                assert_eq!(LIGHT_LEVELS[level as usize].to_bits(), intensity.to_bits());
                for light in [intensity, intensity * 0.72, intensity * 0.86] {
                    let table = shades.table(tint, light);
                    for (channel, tint) in table.iter().zip(tint) {
                        for (c, &actual) in channel.iter().enumerate() {
                            assert_eq!(
                                actual,
                                (c as f64 * f64::from(tint) / 255. * light).clamp(0., 255.) as u8
                            );
                        }
                    }
                }
            }
        }
        assert!(shades.tables.len() <= 128);
        assert_eq!(shades.tables.len(), shades.order.len());
    }

    fn synthetic_world() -> (tempfile::TempDir, World, Vec<[i32; 3]>) {
        let mut blocks = vec![0i8; 4096];
        let mut data = vec![0i8; 2048];
        let mut positions = Vec::new();
        for z in 0..6 {
            for x in 0..6 {
                for y in 0..4 {
                    let id = if y == 0 {
                        1
                    } else if x < 3 {
                        9
                    } else if y == 1 {
                        85
                    } else {
                        0
                    };
                    if id == 0 {
                        continue;
                    }
                    let i = y * 256 + z * 16 + x;
                    blocks[i] = id;
                    positions.push([x as i32, y as i32, z as i32]);
                }
            }
        }
        blocks[3 * 256 + 3 * 16 + 4] = 53;
        data[(3 * 256 + 3 * 16 + 4) / 2] = 2;
        positions.push([4, 3, 3]);
        let mut high_blocks = vec![0i8; 4096];
        high_blocks[15 * 256 + 3 * 16 + 4] = 1;
        positions.push([4, 255, 3]);
        let mut biomes = vec![1i8; 256];
        for z in 0..16 {
            for x in 0..8 {
                biomes[z * 16 + x] = 6;
            }
        }
        let root = nbt!({"DataVersion":1343i32,"Level":{"xPos":0i32,"zPos":0i32,
            "Biomes":ByteArray::new(biomes),"Sections":[{"Y":0i8,
            "Blocks":ByteArray::new(blocks),"Data":ByteArray::new(data),
            "SkyLight":ByteArray::new(vec![-1;2048])},{"Y":15i8,
            "Blocks":ByteArray::new(high_blocks),"SkyLight":ByteArray::new(vec![-1;2048])}]}});
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder
            .write_all(&fastnbt::to_bytes(&root).unwrap())
            .unwrap();
        let compressed = encoder.finish().unwrap();
        let mut bytes = vec![0u8; 12288];
        bytes[..4].copy_from_slice(&[0, 0, 2, 1]);
        bytes[8192..8196].copy_from_slice(&((compressed.len() + 1) as u32).to_be_bytes());
        bytes[8196] = 2;
        bytes[8197..8197 + compressed.len()].copy_from_slice(&compressed);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("r.0.0.mca"), bytes).unwrap();
        let world = World::open(dir.path(), 1024 * 1024).unwrap();
        (dir, world, positions)
    }

    fn test_assets() -> (tempfile::TempDir, Assets) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("blocks")).unwrap();
        for (name, color) in [
            ("stone", [180, 150, 100, 255]),
            ("water_still", [220, 220, 255, 180]),
            ("planks_oak", [120, 80, 40, 255]),
        ] {
            RgbaImage::from_fn(16, 16, |x, y| {
                image::Rgba(if x % 3 == 0 || y % 5 == 0 {
                    [color[0] / 2, color[1] / 2, color[2] / 2, color[3]]
                } else {
                    color
                })
            })
            .save(dir.path().join("blocks").join(format!("{name}.png")))
            .unwrap();
        }
        let assets = Assets::load(dir.path()).unwrap();
        (dir, assets)
    }

    #[test]
    fn neighbor_connections_obey_material_and_gate_orientation() {
        let block = |id, data| Block {
            id,
            data,
            ..Block::default()
        };
        assert!(connects(block(85, 0), block(188, 0), 0, &[]));
        assert!(!connects(block(85, 0), block(113, 0), 0, &[]));
        assert!(connects(block(85, 0), block(107, 0), 0, &[]));
        assert!(!connects(block(85, 0), block(107, 0), 2, &[]));
        assert!(!connects(block(85, 0), block(1, 0), 0, &[(1, None)]));
        assert!(connects(block(102, 0), block(160, 12), 3, &[]));
        assert!(!connects(block(102, 0), block(0, 0), 3, &[]));
    }

    #[test]
    fn tile_chunk_retention_and_biome_grid_are_bounded() {
        let (_dir, world, _) = synthetic_world();
        let (_textures, assets) = test_assets();
        let mut sample = Sample::new(&world);
        let color = sample.tint(&assets, 4, 7, 2);
        let swamp = assets.tint(4, 6);
        let plains = assets.tint(4, 1);
        assert_eq!(
            color,
            std::array::from_fn(
                |i| ((u16::from(swamp[i]) * 6 + u16::from(plains[i]) * 3) / 9) as u8
            )
        );
        assert_eq!(sample.tint(&assets, 4, 7, 2), color);
        assert!(sample.chunk(7, 2).tints[2 * 16 + 7].is_some());
        // The window is direct-mapped, so a chunk 16 apart in both axes evicts
        // its predecessor; sweeping past it must still read correct blocks.
        assert_eq!(sample.slots.len(), TILE_CHUNK_SLOTS);
        for x in 0..TILE_CHUNK_SLOTS * 2 {
            sample.block((x * 16) as i32, 0, 0);
        }
        assert!(sample.slots.iter().filter(|s| s.position.is_some()).count() <= TILE_CHUNK_SLOTS);
        assert_eq!(sample.tint(&assets, 4, 7, 2), color);
        assert!(std::mem::size_of::<Draw>() <= 72);
    }

    #[test]
    fn streaming_matches_unculled_reference_and_tile_edges_all_rotations() {
        let (_dir, world, positions) = synthetic_world();
        let (_textures, assets) = test_assets();
        for view in [View::Topdown, View::Isometric] {
            for rotation in 0..4 {
                let mut map: MapConfig =
                    toml::from_str("id='test'\nworld='test'\nmode='plain'").unwrap();
                map.view = view;
                let front = [(-1., 0.), (0., 1.)].map(|(x, z)| {
                    let (x, z) = rotate(x, z, (4 - rotation) % 4);
                    (x as i32, z as i32)
                });
                let (sx, sy) = project(4.5, 255.5, 3.5, view, rotation);
                let high_tile = ((sx / 128.).floor() as i32, (sy / 128.).floor() as i32);
                for (tx, ty) in [(-1, -1), (-1, 0), (0, -1), (0, 0), high_tile] {
                    let mut reference = RgbaImage::new(128, 128);
                    let mut sample = Sample::new(&world);
                    let mut draws: Vec<_> = positions
                        .iter()
                        .filter_map(|&p| {
                            let b = sample.block(p[0], p[1], p[2]);
                            draw_block(&mut sample, &assets, &map, &[], p, b, front)
                        })
                        .collect();
                    draws.sort_unstable_by_key(|d| {
                        let (rx, rz) = rotate(f64::from(d.x), f64::from(d.z), rotation);
                        ((rz - rx) as i64 + i64::from(d.y), d.y, d.z, d.x)
                    });
                    #[allow(clippy::needless_update, reason = "test painter mirrors the renderer")]
                    let mut painter = Painter {
                        image: &mut reference,
                        assets: &assets,
                        map: &map,
                        rotation,
                        ox: f64::from(tx * 128),
                        oy: f64::from(ty * 128),
                        shades: ShadeCache::default(),
                        stamps: StampCache::default(),
                        sprites: SpriteCache::default(),
                        alphas: AlphaCache::default(),
                    };
                    for d in &draws {
                        painter.draw(d);
                    }
                    let actual = render_tile(&world, &assets, &map, rotation, &[], tx, ty, 128);
                    assert_eq!(
                        actual, reference,
                        "reference mismatch {view:?} rotation {rotation} tile {tx},{ty}"
                    );
                    for dz in 0..2 {
                        for dx in 0..2 {
                            let tile = render_tile(
                                &world,
                                &assets,
                                &map,
                                rotation,
                                &[],
                                tx * 2 + dx,
                                ty * 2 + dz,
                                64,
                            );
                            assert_eq!(
                                tile,
                                image::imageops::crop_imm(
                                    &actual,
                                    (dx * 64) as u32,
                                    (dz * 64) as u32,
                                    64,
                                    64
                                )
                                .to_image(),
                                "tile edge mismatch {view:?} rotation {rotation}"
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn projection_roundtrips() {
        for view in [View::Topdown, View::Isometric] {
            for r in 0..4 {
                for (x, y, z) in [(-123.5, 255., -7.25), (0., 0., 0.), (10., 64., 20.)] {
                    let (sx, sy) = project(x, y, z, view, r);
                    let (a, b) = inverse(sx, sy, y, view, r);
                    assert!((a - x).abs() < 1e-9 && (b - z).abs() < 1e-9);
                }
            }
        }
        assert_eq!(project(1., 0., 0., View::Isometric, 0), (16., -8.));
        assert_eq!(project(0., 1., 0., View::Isometric, 0), (0., -16.));
    }
    #[test]
    fn negative_tile_coverage() {
        let tiles = affected_tiles(-1, -1, View::Topdown, 0, 256);
        assert!(tiles.contains(&(-1, -1)));
        assert!(tiles.contains(&(-2, -2)));
        assert!(tiles.contains(&(0, 0)));
    }
    #[test]
    fn full_height_coverage() {
        for r in 0..4 {
            let tiles = affected_tiles(-3, 7, View::Isometric, r, 256);
            for x in [-48., -32.] {
                for z in [112., 128.] {
                    for y in [0., 128., 256.] {
                        let (sx, sy) = project(x, y, z, View::Isometric, r);
                        assert!(
                            tiles.contains(&(
                                (sx / 256.).floor() as i32,
                                (sy / 256.).floor() as i32
                            ))
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn source_over() {
        let mut p = [0; 4];
        blend(&mut p, [200, 100, 50, 128]);
        assert_eq!(p, [200, 100, 50, 128]);
        blend(&mut p, [0, 0, 250, 128]);
        assert_eq!(p[3], 192);
        assert!((i16::from(p[0]) - 66).abs() <= 1);
        blend(&mut p, [9, 8, 7, 255]);
        assert_eq!(p, [9, 8, 7, 255]);
        blend(&mut p, [255, 255, 255, 0]);
        assert_eq!(p, [9, 8, 7, 255]);
    }
    #[test]
    fn textured_cube_raster_has_no_cracks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("blocks")).unwrap();
        RgbaImage::from_pixel(16, 16, image::Rgba([180, 150, 100, 255]))
            .save(dir.path().join("blocks/stone.png"))
            .unwrap();
        let assets = Assets::load(dir.path()).unwrap();
        let map: MapConfig = toml::from_str("id='test'\nworld='test'\nmode='plain'").unwrap();
        for rotation in 0..4 {
            let mut image = RgbaImage::new(96, 96);
            let b = Block {
                id: 1,
                data: 0,
                block_light: 0,
                sky_light: 15,
                biome: 1,
            };
            let d = Draw {
                depth: 0,
                x: 0,
                y: 0,
                z: 0,
                b,
                tint: [255; 3],
                connections: 0,
                faces: [true; 3],
                light: [1.; 3],
            };
            Painter {
                image: &mut image,
                assets: &assets,
                map: &map,
                rotation,
                ox: -48.,
                oy: -48.,
                shades: ShadeCache::default(),
                stamps: StampCache::default(),
                sprites: SpriteCache::default(),
                alphas: AlphaCache::default(),
            }
            .draw(&d);
            assert_eq!(
                image.pixels().filter(|p| p[3] != 0).count(),
                768,
                "rotation {rotation}"
            );
        }
    }
}
