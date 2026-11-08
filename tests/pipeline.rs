//! Synthetic 1.12.2 worlds exercise the public publication pipeline, not assets internals.
use fastnbt::{ByteArray, nbt};
use flate2::{Compression, write::ZlibEncoder};
use image::{Rgba, RgbaImage};
use rayon::prelude::*;
use serde::Deserialize;
use std::{collections::BTreeMap, fs, io::Write, path::Path};
use tempfile::TempDir;
use voxelatlas::{
    config::{Config, Dimension, MapConfig, Mode, View, WorldConfig},
    output::{self, Manifest},
};

const STONE: [u8; 4] = [180, 150, 100, 255];
const DIRT: [u8; 4] = [90, 50, 25, 255];

struct Fixture {
    _temp: TempDir,
    config: Config,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let world = root.join("world");
        let textures = root.join("textures");
        fs::create_dir_all(world.join("region")).unwrap();
        fs::create_dir_all(textures.join("blocks")).unwrap();
        for (name, color) in [
            ("stone", STONE),
            ("dirt", DIRT),
            ("grass_top", [100, 200, 70, 255]),
            ("grass_side", [110, 140, 60, 255]),
            ("water_still", [255, 255, 255, 180]),
            ("water_flow", [255, 255, 255, 180]),
            ("sand", [230, 220, 160, 255]),
        ] {
            RgbaImage::from_pixel(16, 16, Rgba(color))
                .save(textures.join("blocks").join(format!("{name}.png")))
                .unwrap();
        }
        let config = Config {
            encoding: voxelatlas::config::Encoding::Fast,
            output: root.join("output"),
            textures,
            workers: 4,
            cache_mib: 16,
            tile_size: 256,
            worlds: BTreeMap::from([(
                "world".into(),
                WorldConfig {
                    path: world,
                    dimension: Dimension::Overworld,
                    exclude: vec![],
                },
            )]),
            maps: vec![MapConfig {
                id: "surface".into(),
                world: "world".into(),
                view: View::Topdown,
                rotations: vec![0],
                mode: Mode::Plain,
                water_opacity: 0.8,
                lighting_water_intensity: 0.5,
                default_zoom: 0,
            }],
        };
        config.validate().unwrap();
        Self {
            _temp: temp,
            config,
        }
    }

    // Rewrite regions atomically, so fingerprint invalidation does not depend on
    // timestamp precision. Each listed chunk contains exactly one exposed cube.
    fn world(&self, chunks: &[(i32, i32, i8)]) {
        self.world_with_tick(chunks, 1);
    }
    fn world_with_tick(&self, chunks: &[(i32, i32, i8)], tick: i64) {
        let directory = self.config.worlds["world"].path.join("region");
        for entry in fs::read_dir(&directory).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        let mut regions = BTreeMap::<(i32, i32), Vec<u8>>::new();
        for &(x, z, id) in chunks {
            let mut blocks = vec![0i8; 4096];
            blocks[8 * 16 + 8] = id;
            let root = nbt!({
                "DataVersion": 1343i32,
                "Level": {
                    "xPos": x, "zPos": z, "LastUpdate": tick,
                    "Biomes": ByteArray::new(vec![1; 256]),
                    "Sections": [{
                        "Y": 0i8, "Blocks": ByteArray::new(blocks),
                        "Data": ByteArray::new(vec![0; 2048]),
                        "BlockLight": ByteArray::new(vec![0; 2048]),
                        "SkyLight": ByteArray::new(vec![-1; 2048]),
                    }],
                },
            });
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
            encoder
                .write_all(&fastnbt::to_bytes(&root).unwrap())
                .unwrap();
            let payload = encoder.finish().unwrap();
            let region = regions
                .entry((x.div_euclid(32), z.div_euclid(32)))
                .or_insert_with(|| vec![0; 8192]);
            let sector = region.len() / 4096;
            let sectors = (payload.len() + 5).div_ceil(4096);
            assert!(sectors <= 255);
            let slot = (z.rem_euclid(32) * 32 + x.rem_euclid(32)) as usize * 4;
            let offset = (sector as u32).to_be_bytes();
            region[slot..slot + 3].copy_from_slice(&offset[1..]);
            region[slot + 3] = sectors as u8;
            region[4096 + slot..4100 + slot].copy_from_slice(&1u32.to_be_bytes());
            let start = region.len();
            region.resize(start + sectors * 4096, 0);
            region[start..start + 4].copy_from_slice(&((payload.len() + 1) as u32).to_be_bytes());
            region[start + 4] = 2;
            region[start + 5..start + 5 + payload.len()].copy_from_slice(&payload);
        }
        for ((x, z), bytes) in regions {
            output::atomic_write(&directory.join(format!("r.{x}.{z}.mca")), &bytes).unwrap();
        }
    }

    fn full_config(&self, name: &str) -> Config {
        let mut config = self.config.clone();
        config.output = self._temp.path().join(name);
        config
    }
}

fn manifest(output: &Path) -> Manifest {
    let value = serde_json::from_slice::<serde_json::Value>(
        &fs::read(output.join("current.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(value["version"], 2);
    for map in value["maps"].as_array().unwrap() {
        assert!(
            map.get("tiles").is_none(),
            "v2 must publish sharded indexes"
        );
    }
    serde_json::from_value(value).unwrap()
}

type TileSet = BTreeMap<(String, u8, String), String>;

fn tiles(output: &Path) -> TileSet {
    #[derive(Deserialize)]
    struct Index {
        tiles: BTreeMap<String, String>,
    }
    let mut result = BTreeMap::new();
    for map in manifest(output).maps {
        for (shard, hash) in map.tile_indexes {
            let bytes = fs::read(output.join("indexes").join(format!("{hash}.json"))).unwrap();
            assert_eq!(blake3::hash(&bytes).to_hex().as_str(), hash);
            let index: Index = serde_json::from_slice(&bytes).unwrap();
            assert!(!index.tiles.is_empty());
            for (key, hash) in index.tiles {
                let coords: Vec<i32> = key.split('/').map(|s| s.parse().unwrap()).collect();
                assert_eq!(coords.len(), 3);
                assert_eq!(
                    shard,
                    format!(
                        "{}/{}/{}",
                        coords[0],
                        coords[1].div_euclid(32),
                        coords[2].div_euclid(32)
                    )
                );
                let bytes = fs::read(output.join("tiles").join(format!("{hash}.webp"))).unwrap();
                assert_eq!(blake3::hash(&bytes).to_hex().as_str(), hash);
                assert!(
                    result
                        .insert((map.id.clone(), map.rotation, key), hash)
                        .is_none()
                );
            }
        }
    }
    result
}

fn decoded(output: &Path) -> BTreeMap<(String, u8, String), RgbaImage> {
    tiles(output)
        .into_iter()
        .map(|(key, hash)| {
            let image = image::open(output.join("tiles").join(format!("{hash}.webp")))
                .unwrap()
                .into_rgba8();
            assert_eq!(image.dimensions(), (256, 256));
            assert!(image.pixels().any(|p| p[3] != 0));
            (key, image)
        })
        .collect()
}

#[test]
fn first_render_noop_and_incremental_match_forced_pixels() {
    let fixture = Fixture::new();
    fixture.world(&[(0, 0, 1)]);
    let first = output::render(&fixture.config, false).unwrap();
    assert!(!first.unchanged);
    assert!(first.base_tiles_rendered > 0);
    let before = decoded(&fixture.config.output);
    assert_eq!(before.len(), 1);
    assert_eq!(before.values().next().unwrap().get_pixel(136, 136).0, STONE);
    let current = fs::read(fixture.config.output.join("current.json")).unwrap();
    let state = fs::read(fixture.config.output.join(".state/index.json")).unwrap();
    let second = output::render(&fixture.config, false).unwrap();
    assert!(second.unchanged);
    assert_eq!(
        (
            second.base_tiles_rendered,
            second.parent_tiles_rendered,
            second.chunks_changed
        ),
        (0, 0, 0)
    );
    assert_eq!(
        fs::read(fixture.config.output.join("current.json")).unwrap(),
        current
    );
    assert_eq!(
        fs::read(fixture.config.output.join(".state/index.json")).unwrap(),
        state
    );
    fixture.world(&[(0, 0, 3)]);
    let changed = output::render(&fixture.config, false).unwrap();
    assert_eq!(changed.chunks_changed, 1);
    let after = decoded(&fixture.config.output);
    assert_ne!(before, after);
    assert_eq!(after.values().next().unwrap().get_pixel(136, 136).0, DIRT);
    let full = fixture.full_config("full");
    output::render(&full, true).unwrap();
    assert_eq!(after, decoded(&full.output));
}

#[test]
fn saved_ticks_do_not_rerender_but_block_changes_still_do() {
    let fixture = Fixture::new();
    fixture.world_with_tick(&[(0, 0, 1)], 1);
    output::render(&fixture.config, false).unwrap();
    let public = fs::read(fixture.config.output.join("current.json")).unwrap();
    fixture.world_with_tick(&[(0, 0, 1)], 9999);
    let ticks = output::render(&fixture.config, false).unwrap();
    assert!(ticks.unchanged);
    assert_eq!(ticks.metadata_only_chunks, 1);
    assert_eq!(ticks.base_tiles_rendered, 0);
    assert_eq!(
        public,
        fs::read(fixture.config.output.join("current.json")).unwrap()
    );
    assert!(output::render(&fixture.config, false).unwrap().unchanged);
    fixture.world_with_tick(&[(0, 0, 3)], 10000);
    assert_eq!(
        output::render(&fixture.config, false)
            .unwrap()
            .chunks_changed,
        1
    );
}

#[test]
fn map_and_presentation_changes_only_invalidate_affected_views() {
    let mut fixture = Fixture::new();
    fixture.world(&[(0, 0, 1)]);
    let first = output::render(&fixture.config, false).unwrap();
    let mut added = fixture.config.maps[0].clone();
    added.id = "second".into();
    fixture.config.maps.push(added);
    let new = output::render(&fixture.config, false).unwrap();
    assert_eq!(new.base_tiles_rendered, first.base_tiles_rendered);
    assert_eq!(manifest(&fixture.config.output).maps.len(), 2);
    fixture.config.maps[1].default_zoom = 3;
    let presentation = output::render(&fixture.config, false).unwrap();
    assert!(!presentation.unchanged);
    assert_eq!(presentation.base_tiles_rendered, 0);
    assert_eq!(manifest(&fixture.config.output).maps[1].default_zoom, 3);
    fixture.config.maps[1].water_opacity = 0.5;
    assert_eq!(
        output::render(&fixture.config, false)
            .unwrap()
            .base_tiles_rendered,
        first.base_tiles_rendered
    );
    fixture.config.maps.pop();
    let removed = output::render(&fixture.config, false).unwrap();
    assert_eq!(removed.base_tiles_rendered, 0);
    assert_eq!(manifest(&fixture.config.output).maps.len(), 1);
}

#[test]
fn switching_equal_world_alias_updates_marker_group_without_raster_work() {
    let mut fixture = Fixture::new();
    fixture.world(&[(0, 0, 1)]);
    fixture
        .config
        .worlds
        .insert("alias".into(), fixture.config.worlds["world"].clone());
    output::render(&fixture.config, false).unwrap();
    fixture.config.maps[0].world = "alias".into();
    let result = output::render(&fixture.config, false).unwrap();
    assert_eq!(result.base_tiles_rendered, 0);
    assert_eq!(manifest(&fixture.config.output).maps[0].world, "alias");
}

#[test]
fn compact_encoding_preserves_every_decoded_pixel_and_zoom() {
    let mut fixture = Fixture::new();
    fixture.world(&[(0, 0, 1), (3, 1, 3)]);
    fixture.config.maps[0].view = View::Isometric;
    output::render(&fixture.config, false).unwrap();
    let pixels = decoded(&fixture.config.output);
    let mut compact = fixture.full_config("compact");
    compact.encoding = voxelatlas::config::Encoding::Compact;
    output::render(&compact, false).unwrap();
    assert_eq!(pixels, decoded(&compact.output));
}

#[test]
fn negative_coordinates_growth_zoom_transitions_and_deletion() {
    let fixture = Fixture::new();
    fixture.world(&[(0, 0, 1)]);
    output::render(&fixture.config, false).unwrap();
    assert_eq!(manifest(&fixture.config.output).maps[0].min_zoom, 0);
    fixture.world(&[(0, 0, 1), (-32, 0, 3)]);
    output::render(&fixture.config, false).unwrap();
    let grown = manifest(&fixture.config.output);
    assert!(grown.maps[0].min_zoom < 0);
    let grown_tiles = tiles(&fixture.config.output);
    assert!(grown_tiles.keys().any(|(_, _, key)| key == "0/-32/0"));
    let full = fixture.full_config("grown-full");
    output::render(&full, true).unwrap();
    assert_eq!(decoded(&fixture.config.output), decoded(&full.output));
    fixture.world(&[(0, 0, 1)]);
    output::render(&fixture.config, false).unwrap();
    assert_eq!(manifest(&fixture.config.output).maps[0].min_zoom, 0);
    assert_eq!(
        tiles(&fixture.config.output).len(),
        1,
        "deleted chunk and obsolete zooms must lose every reference"
    );
    fixture.world(&[]);
    output::render(&fixture.config, false).unwrap();
    assert!(tiles(&fixture.config.output).is_empty());
    assert!(
        manifest(&fixture.config.output).maps[0]
            .tile_indexes
            .is_empty()
    );
    let empty = fixture.full_config("empty-full");
    output::render(&empty, true).unwrap();
    assert_eq!(decoded(&fixture.config.output), decoded(&empty.output));
}

#[test]
fn worker_counts_produce_identical_content_hashes() {
    let fixture = Fixture::new();
    fixture.world(&[(-1, -1, 1), (0, 0, 3), (1, 0, 1), (2, 0, 1)]);
    let mut single = fixture.full_config("single");
    single.workers = 1;
    output::render(&single, true).unwrap();
    output::render(&fixture.config, true).unwrap();
    assert_eq!(tiles(&single.output), tiles(&fixture.config.output));
    assert_eq!(
        manifest(&single.output).generation,
        manifest(&fixture.config.output).generation
    );
}

#[test]
fn identical_hash_atomic_writes_are_safe_under_contention() {
    let temp = tempfile::tempdir().unwrap();
    let bytes = vec![0x5a; 64 * 1024];
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let path = temp.path().join(format!("{hash}.webp"));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .build()
        .unwrap();
    pool.install(|| {
        (0..128).into_par_iter().for_each(|_| {
            output::atomic_write(&path, &bytes).unwrap();
            assert_eq!(fs::read(&path).unwrap(), bytes);
        })
    });
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(
        fs::read_dir(temp.path()).unwrap().count(),
        1,
        "temporary files must not leak"
    );
}

#[test]
fn corrupt_chunk_does_not_replace_published_generation_or_state() {
    let fixture = Fixture::new();
    fixture.world(&[(0, 0, 1)]);
    output::render(&fixture.config, false).unwrap();
    let current = fs::read(fixture.config.output.join("current.json")).unwrap();
    let state = fs::read(fixture.config.output.join(".state/index.json")).unwrap();
    let old_tiles = decoded(&fixture.config.output);
    let region = fixture.config.worlds["world"].path.join("region/r.0.0.mca");
    let mut bytes = fs::read(&region).unwrap();
    // Preserve a valid Anvil allocation/header but break the compressed stream.
    bytes[8197..8201].copy_from_slice(&[0xff; 4]);
    output::atomic_write(&region, &bytes).unwrap();
    let error = output::render(&fixture.config, false).unwrap_err();
    assert!(
        format!("{error:#}").contains("unreadable chunk"),
        "{error:#}"
    );
    assert_eq!(
        fs::read(fixture.config.output.join("current.json")).unwrap(),
        current
    );
    assert_eq!(
        fs::read(fixture.config.output.join(".state/index.json")).unwrap(),
        state
    );
    assert_eq!(decoded(&fixture.config.output), old_tiles);
}

#[test]
fn gc_keeps_current_and_grace_period_indexes_and_tiles() {
    let fixture = Fixture::new();
    fixture.world(&[(0, 0, 1)]);
    output::render(&fixture.config, false).unwrap();
    let old = manifest(&fixture.config.output);
    let old_tiles = tiles(&fixture.config.output);
    fixture.world(&[(0, 0, 3)]);
    output::render(&fixture.config, false).unwrap();
    let current = fs::read(fixture.config.output.join("current.json")).unwrap();
    assert_eq!(output::gc(&fixture.config.output, 1, 3600).unwrap(), 0);
    assert!(
        fixture
            .config
            .output
            .join("generations")
            .join(format!("{}.json", old.generation))
            .exists()
    );
    for hash in old.maps.iter().flat_map(|m| m.tile_indexes.values()) {
        assert!(
            fixture
                .config
                .output
                .join("indexes")
                .join(format!("{hash}.json"))
                .exists()
        );
    }
    for hash in old_tiles.values() {
        assert!(
            fixture
                .config
                .output
                .join("tiles")
                .join(format!("{hash}.webp"))
                .exists()
        );
    }
    // Unreferenced immutable objects are collected without relying on mtime order.
    let orphan_hash = "0".repeat(64);
    let orphan_index = fixture
        .config
        .output
        .join("indexes")
        .join(format!("{orphan_hash}.json"));
    let orphan_tile = fixture
        .config
        .output
        .join("tiles")
        .join(format!("{orphan_hash}.webp"));
    fs::write(&orphan_index, b"{\"tiles\":{}}").unwrap();
    fs::write(&orphan_tile, b"orphan").unwrap();
    assert!(output::gc(&fixture.config.output, 1, 0).unwrap() >= 1);
    assert!(!orphan_index.exists());
    assert!(!orphan_tile.exists());
    assert_eq!(
        fs::read(fixture.config.output.join("current.json")).unwrap(),
        current
    );
    assert!(!decoded(&fixture.config.output).is_empty());
}

#[test]
fn all_isometric_rotations_have_known_pixels_and_match_forced_render() {
    let mut fixture = Fixture::new();
    fixture.config.maps[0].view = View::Isometric;
    fixture.config.maps[0].rotations = vec![0, 1, 2, 3];
    fixture.world(&[(0, 0, 1)]);
    output::render(&fixture.config, false).unwrap();
    fixture.world(&[(0, 0, 3)]);
    output::render(&fixture.config, false).unwrap();
    let incremental = decoded(&fixture.config.output);
    for rotation in 0..4 {
        let (x, y) = voxelatlas::render::project(8.5, 1., 8.5, View::Isometric, rotation);
        let (x, y) = (x.floor() as i32, y.floor() as i32);
        let key = (
            "surface".into(),
            rotation,
            format!("0/{}/{}", x.div_euclid(256), y.div_euclid(256)),
        );
        assert_eq!(
            incremental[&key]
                .get_pixel(x.rem_euclid(256) as u32, y.rem_euclid(256) as u32)
                .0,
            DIRT,
            "rotation {rotation}"
        );
    }
    let full = fixture.full_config("iso-full");
    output::render(&full, true).unwrap();
    assert_eq!(incremental, decoded(&full.output));
}
