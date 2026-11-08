//! Java 1.12 texture lookup. Textures are decoded once, never in the render loop.
use anyhow::{Context, Result, bail};
use image::{RgbaImage, imageops::FilterType};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    Cube,
    Slab,
    Stair,
    Plant,
    Thin,
    Snow,
    Fence,
    Air,
}
#[derive(Clone, Copy)]
pub(crate) struct Material {
    pub top: usize,
    pub side: usize,
    /// West, east, north, south, in world coordinates (not camera coordinates).
    pub sides: [usize; 4],
    pub top_turns: u8,
    pub shape: Shape,
    pub tint: u8,
}
pub struct Assets {
    textures: Vec<RgbaImage>,
    materials: Vec<Material>,
    grass: Option<RgbaImage>,
    foliage: Option<RgbaImage>,
    fingerprint: String,
    warned: Vec<AtomicBool>,
    unknown: Vec<bool>,
}
impl Assets {
    pub fn load(directory: &Path) -> Result<Self> {
        let root = [
            directory.join("assets/minecraft/textures"),
            directory.join("textures"),
            directory.to_owned(),
        ]
        .into_iter()
        .find(|p| p.join("blocks").is_dir())
        .context("texture directory must contain blocks/ (or assets/minecraft/textures/blocks/)")?;
        let mut files =
            std::fs::read_dir(root.join("blocks"))?.collect::<std::io::Result<Vec<_>>>()?;
        files.sort_by_key(|e| e.file_name());
        let mut hash = blake3::Hasher::new();
        hash.update(b"voxelatlas-legacy-textures-v4");
        let missing = RgbaImage::from_fn(16, 16, |x, y| {
            image::Rgba(if (x / 4 + y / 4) % 2 == 0 {
                [255, 0, 255, 255]
            } else {
                [25, 0, 25, 255]
            })
        });
        let mut textures = vec![missing];
        let mut names = HashMap::new();
        for file in files {
            let path = file.path();
            if path.extension().and_then(|s| s.to_str()) != Some("png") {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            hash.update(file.file_name().to_string_lossy().as_bytes());
            hash.update(&bytes);
            let img = image::load_from_memory(&bytes)
                .with_context(|| format!("decoding {}", path.display()))?
                .to_rgba8();
            if img.width() == 0 || img.height() < img.width() {
                bail!("invalid block texture {}", path.display());
            }
            // Animated strips use their first square frame, then nearest-neighbor sampling.
            let frame = image::imageops::crop_imm(&img, 0, 0, img.width(), img.width()).to_image();
            names.insert(
                path.file_stem()
                    .unwrap()
                    .to_string_lossy()
                    .trim_end()
                    .to_owned(),
                textures.len(),
            );
            textures.push(image::imageops::resize(&frame, 16, 16, FilterType::Nearest));
        }
        if names.is_empty() {
            bail!("no PNG block textures found in {}", root.display());
        }
        // Chests moved out of blocks/ in vanilla. Use their actual entity atlas
        // for a single-chest cuboid approximation, not misleading oak planks.
        for (kind, file) in [
            ("chest", "normal"),
            ("trapped_chest", "trapped"),
            ("ender_chest", "ender"),
        ] {
            let path = root.join("entity/chest").join(format!("{file}.png"));
            if !path.exists() {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            hash.update(kind.as_bytes());
            hash.update(&bytes);
            let atlas = image::load_from_memory(&bytes)?.to_rgba8();
            if atlas.width() < 64 || atlas.width() % 64 != 0 || atlas.height() < atlas.width() {
                continue;
            }
            let scale = atlas.width() / 64;
            let crop = |x, y, w, h| {
                image::imageops::crop_imm(&atlas, x * scale, y * scale, w * scale, h * scale)
                    .to_image()
            };
            let top = image::imageops::resize(&crop(14, 0, 14, 14), 16, 16, FilterType::Nearest);
            let mut side = RgbaImage::new(14, 15);
            image::imageops::replace(
                &mut side,
                &image::imageops::resize(&crop(14, 14, 14, 5), 14, 5, FilterType::Nearest),
                0,
                0,
            );
            image::imageops::replace(
                &mut side,
                &image::imageops::resize(&crop(14, 33, 14, 10), 14, 10, FilterType::Nearest),
                0,
                5,
            );
            for (suffix, img) in [
                ("top", top),
                (
                    "side",
                    image::imageops::resize(&side, 16, 16, FilterType::Nearest),
                ),
            ] {
                names.insert(format!("{kind}_{suffix}"), textures.len());
                textures.push(img);
            }
        }
        let mut maps = Vec::new();
        for name in ["grass", "foliage"] {
            let path = root.join("colormap").join(format!("{name}.png"));
            maps.push(if path.exists() {
                let bytes = std::fs::read(path)?;
                hash.update(name.as_bytes());
                hash.update(&bytes);
                Some(image::load_from_memory(&bytes)?.to_rgba8())
            } else {
                None
            });
        }
        let mut materials = Vec::with_capacity(4096 * 16);
        let mut unknown = Vec::with_capacity(4096 * 16);
        let mut missing_count = 0;
        for id in 0..4096 {
            for data in 0..16 {
                let (top, side, _bottom) = texture_names(id, data);
                let (top, sides) = directional_names(id, data, top, &side);
                let index = |s: &str| names.get(s).copied().unwrap_or(0);
                let shape = shape(id);
                let absent =
                    top == "missing" || index(&top) == 0 || sides.iter().any(|s| index(s) == 0);
                if absent && shape != Shape::Air {
                    missing_count += 1;
                }
                unknown.push(absent);
                materials.push(Material {
                    top: index(&top),
                    side: index(&side),
                    sides: sides.each_ref().map(|s| index(s)),
                    top_turns: if matches!(id, 93 | 94 | 149 | 150) {
                        (data + 2) & 3
                    } else {
                        0
                    },
                    shape,
                    tint: match id {
                        2 => 1,
                        18 | 161 | 106 => 2,
                        31 if data != 0 => 3,
                        175 if matches!(data & 7, 2 | 3) => 3,
                        8 | 9 => 4,
                        _ => 0,
                    },
                });
            }
        }
        tracing::info!(
            textures = names.len(),
            "loaded block textures (animated textures use first frame)"
        );
        if missing_count > 0 {
            tracing::info!(
                "Unmapped or unavailable textures use magenta checks; warnings identify blocks encountered while rendering. Complex models are approximated, not resource-pack model compatible."
            );
        }
        Ok(Self {
            textures,
            materials,
            grass: maps.remove(0),
            foliage: maps.remove(0),
            fingerprint: hash.finalize().to_hex().to_string(),
            warned: (0..4096).map(|_| AtomicBool::new(false)).collect(),
            unknown,
        })
    }
    pub fn fingerprint(&self) -> String {
        self.fingerprint.clone()
    }
    pub(crate) fn material(&self, id: u16, data: u8) -> Material {
        let id = usize::from(id).min(4095);
        let idx = id * 16 + usize::from(data & 15);
        if self.unknown[idx] && id != 0 && !self.warned[id].swap(true, Ordering::Relaxed) {
            tracing::warn!(
                id,
                data,
                "missing block texture/model: rendering magenta fallback"
            );
        }
        self.materials[idx]
    }
    /// Row-major 16x16 texels, so the rasterizer can index a cached stamp
    /// directly instead of repeating `pixel`'s float-to-texel conversion.
    /// Every loaded texture is resized to 16x16, which `pixel` already assumes.
    pub(crate) fn texels(&self, texture: usize) -> &[[u8; 4]; 256] {
        let (texels, rest) = self.textures[texture].as_chunks::<4>();
        debug_assert!(rest.is_empty() && texels.len() == 256);
        texels.try_into().expect("textures are 16x16 RGBA")
    }
    pub(crate) fn tint(&self, kind: u8, biome: u8) -> [u8; 3] {
        if kind == 4 {
            return if biome == 6 {
                [97, 123, 71]
            } else {
                [63, 118, 228]
            };
        }
        if biome == 6 {
            return [106, 112, 57];
        }
        let (temp, rain): (f64, f64) = match biome {
            2 | 17 | 130 => (1., 0.),
            4 | 18 | 27 | 28 | 29 | 157 => (0.7, 0.8),
            21..=23 | 149 | 151 => (0.95, 0.9),
            5 | 19 | 30..=33 | 133 | 158 | 160 | 161 => (0.25, 0.8),
            10..=13 | 26 | 140 => (0., 0.5),
            35 | 36 | 163 | 164 => (1., 0.),
            37..=39 | 165..=167 => (1., 0.),
            _ => (0.8, 0.4),
        };
        let map = if kind == 2 {
            self.foliage.as_ref()
        } else {
            self.grass.as_ref()
        };
        if let Some(map) = map {
            let x = ((1. - temp) * (map.width() - 1) as f64) as u32;
            let y = ((1. - rain * temp) * (map.height() - 1) as f64) as u32;
            let p = map.get_pixel(x, y);
            [p[0], p[1], p[2]]
        } else if kind == 2 {
            [95, 145, 65]
        } else {
            [120, 171, 75]
        }
    }
}
/// Block ids are 12-bit, so the shape and transparency classifications fit in
/// 4 KiB tables. The rasterizer consults them per sampled block; a direct index
/// is far cheaper than re-walking the id ranges. Tables are built from the same
/// match expressions, so they classify identically.
static SHAPES: LazyLock<[Shape; 4096]> =
    LazyLock::new(|| std::array::from_fn(|id| shape_of(id as u16)));
static TRANSPARENT: LazyLock<[bool; 4096]> =
    LazyLock::new(|| std::array::from_fn(|id| transparent_id(id as u16)));
pub(crate) fn shape(id: u16) -> Shape {
    SHAPES[usize::from(id).min(4095)]
}
pub(crate) fn transparent(id: u16) -> bool {
    TRANSPARENT[usize::from(id).min(4095)]
}
fn shape_of(id: u16) -> Shape {
    if matches!(id, 34 | 55 | 69 | 77 | 131 | 132 | 143) {
        return Shape::Thin;
    }
    match id {
        0 | 36 | 166 | 217 => Shape::Air,
        44 | 126 | 182 | 205 => Shape::Slab,
        53 | 67 | 108 | 109 | 114 | 128 | 134..=136 | 156 | 163 | 164 | 180 | 203 => Shape::Stair,
        27 | 28 | 66 | 70 | 72 | 93 | 94 | 96 | 111 | 147..=150 | 157 | 167 | 171 => Shape::Thin,
        78 => Shape::Snow,
        6
        | 30..=32
        | 37..=40
        | 50
        | 51
        | 59
        | 63
        | 65
        | 68
        | 69
        | 75..=77
        | 83
        | 90
        | 104..=106
        | 115
        | 119
        | 127
        | 131
        | 132
        | 140..=143
        | 175
        | 176
        | 177
        | 198
        | 207
        | 209 => Shape::Plant,
        85 | 101 | 102 | 113 | 139 | 160 | 188..=192 => Shape::Fence,
        _ => Shape::Cube,
    }
}
fn transparent_id(id: u16) -> bool {
    matches!(id,0|6|8..=11|18|20|26|27|28|30..=32|37..=40|44|50..=55|59|63..=72|75..=78|79|81|83|85|90|92..=96|101..=107|111|113|114|115|117..=120|126..=128|130..=132|134..=136|138..=151|154|156..=164|167|171|175..=178|180|182|183..=200|203|205|207|208|209|212)
}
fn texture_names(id: u16, d: u8) -> (String, String, String) {
    let wood = ["oak", "spruce", "birch", "jungle", "acacia", "big_oak"];
    let colors = [
        "white",
        "orange",
        "magenta",
        "light_blue",
        "yellow",
        "lime",
        "pink",
        "gray",
        "silver",
        "cyan",
        "purple",
        "blue",
        "brown",
        "green",
        "red",
        "black",
    ];
    let one = |s: &str| (s.to_owned(), s.to_owned(), s.to_owned());
    let faces = |t: &str, s: &str, b: &str| (t.to_owned(), s.to_owned(), b.to_owned());
    match id {
        6 if d & 7 == 5 => return one("sapling_roofed_oak"),
        59 => return one(&format!("wheat_stage_{}", d.min(7))),
        115 => {
            return one(&format!(
                "nether_wart_stage_{}",
                if d >= 3 {
                    2
                } else if d >= 1 {
                    1
                } else {
                    0
                }
            ));
        }
        141 | 142 => {
            return one(&format!(
                "{}_stage_{}",
                if id == 141 { "carrots" } else { "potatoes" },
                if d >= 7 { 3 } else { (d / 2).min(2) }
            ));
        }
        207 => return one(&format!("beetroots_stage_{}", d.min(3))),
        34 => {
            return faces(
                if d & 8 != 0 {
                    "piston_top_sticky"
                } else {
                    "piston_top_normal"
                },
                "piston_side",
                "piston_side",
            );
        }
        55 => return one("redstone_dust_line0"),
        69 => return one("lever"),
        77 => return one("stone"),
        93 | 94 => {
            return faces(
                if id == 93 {
                    "repeater_off"
                } else {
                    "repeater_on"
                },
                "stone_slab_side",
                "stone_slab_top",
            );
        }
        131 => return one("trip_wire_source"),
        132 => return one("trip_wire"),
        143 => return one("planks_oak"),
        149 | 150 => {
            return faces(
                if id == 150 || d & 8 != 0 {
                    "comparator_on"
                } else {
                    "comparator_off"
                },
                "stone_slab_side",
                "stone_slab_top",
            );
        }
        54 => return faces("chest_top", "chest_side", "chest_top"),
        130 => return faces("ender_chest_top", "ender_chest_side", "ender_chest_top"),
        146 => {
            return faces(
                "trapped_chest_top",
                "trapped_chest_side",
                "trapped_chest_top",
            );
        }
        120 => return faces("endframe_top", "endframe_side", "end_stone"),
        23 | 158 => return faces("furnace_top", "furnace_side", "furnace_top"),
        64 | 71 | 193..=197 => {
            let door = match id {
                71 => "iron",
                193 => "spruce",
                194 => "birch",
                195 => "jungle",
                196 => "acacia",
                197 => "dark_oak",
                _ => "wood",
            };
            return one(&format!(
                "door_{door}_{}",
                if d & 8 != 0 { "upper" } else { "lower" }
            ));
        }
        92 => return faces("cake_top", "cake_side", "cake_bottom"),
        117 => return one("brewing_stand"),
        118 => return faces("cauldron_top", "cauldron_side", "cauldron_bottom"),
        127 => return one(&format!("cocoa_stage_{}", (d >> 2).min(2))),
        140 => return one("flower_pot"),
        151 | 178 => {
            return faces(
                if id == 151 {
                    "daylight_detector_top"
                } else {
                    "daylight_detector_inverted_top"
                },
                "daylight_detector_side",
                "daylight_detector_side",
            );
        }
        154 => return faces("hopper_top", "hopper_outside", "hopper_outside"),
        _ => {}
    }
    match id {
        0 => one("air"),
        1 => one([
            "stone",
            "stone_granite",
            "stone_granite_smooth",
            "stone_diorite",
            "stone_diorite_smooth",
            "stone_andesite",
            "stone_andesite_smooth",
        ][usize::from(d.min(6))]),
        2 => faces("grass_top", "grass_side", "dirt"),
        3 => one(match d {
            1 => "coarse_dirt",
            2 => "dirt_podzol_top",
            _ => "dirt",
        }),
        4 => one("cobblestone"),
        5 => one(&format!("planks_{}", wood[usize::from(d.min(5))])),
        6 => one(&format!("sapling_{}", wood[usize::from(d.min(5))])),
        7 => one("bedrock"),
        8 | 9 => one("water_still"),
        10 | 11 => one("lava_still"),
        12 => one(if d == 1 { "red_sand" } else { "sand" }),
        13 => one("gravel"),
        14 => one("gold_ore"),
        15 => one("iron_ore"),
        16 => one("coal_ore"),
        17 | 162 => {
            let w = wood[if id == 17 {
                usize::from(d & 3)
            } else {
                4 + usize::from(d & 1)
            }];
            faces(
                &format!("log_{w}_top"),
                &format!("log_{w}"),
                &format!("log_{w}_top"),
            )
        }
        18 | 161 => one(&format!(
            "leaves_{}",
            wood[if id == 18 {
                usize::from(d & 3)
            } else {
                4 + usize::from(d & 1)
            }]
        )),
        19 => one(if d == 1 { "sponge_wet" } else { "sponge" }),
        20 => one("glass"),
        21 => one("lapis_ore"),
        22 => one("lapis_block"),
        23 | 158 => faces("furnace_top", "dispenser_front_horizontal", "furnace_top"),
        24 => faces(
            "sandstone_top",
            if d == 1 {
                "sandstone_carved"
            } else if d == 2 {
                "sandstone_smooth"
            } else {
                "sandstone_normal"
            },
            "sandstone_bottom",
        ),
        25 => one("noteblock"),
        26 => one("bed_feet_top"),
        27 => one("rail_golden"),
        28 => one("rail_detector"),
        29 | 33 => one("piston_side"),
        30 => one("web"),
        31 => one(match d {
            0 => "deadbush",
            2 => "fern",
            _ => "tallgrass",
        }),
        32 => one("deadbush"),
        35 => one(&format!("wool_colored_{}", colors[usize::from(d)])),
        37 => one("flower_dandelion"),
        38 => one([
            "flower_rose",
            "flower_blue_orchid",
            "flower_allium",
            "flower_houstonia",
            "flower_tulip_red",
            "flower_tulip_orange",
            "flower_tulip_white",
            "flower_tulip_pink",
            "flower_oxeye_daisy",
        ][usize::from(d.min(8))]),
        39 => one("mushroom_brown"),
        40 => one("mushroom_red"),
        41 => one("gold_block"),
        42 => one("iron_block"),
        43 | 44 => {
            let s = [
                "stone_slab_side",
                "sandstone_normal",
                "planks_oak",
                "cobblestone",
                "brick",
                "stonebrick",
                "nether_brick",
                "quartz_block_side",
            ][usize::from(d & 7)];
            if d & 7 == 0 {
                faces("stone_slab_top", s, "stone_slab_top")
            } else {
                one(s)
            }
        }
        45 | 108 => one("brick"),
        46 => faces("tnt_top", "tnt_side", "tnt_bottom"),
        47 => faces("planks_oak", "bookshelf", "planks_oak"),
        48 => one("cobblestone_mossy"),
        49 => one("obsidian"),
        50 => one("torch_on"),
        51 => one("fire_layer_0"),
        52 => one("mob_spawner"),
        53 | 85 | 107 => one("planks_oak"),
        54 | 146 => one("planks_oak"),
        55 => one("redstone_dust_line0"),
        56 => one("diamond_ore"),
        57 => one("diamond_block"),
        58 => faces("crafting_table_top", "crafting_table_side", "planks_oak"),
        59 => one(&format!("wheat_stage{}", d.min(7))),
        60 => faces(
            if d == 0 {
                "farmland_dry"
            } else {
                "farmland_wet"
            },
            "dirt",
            "dirt",
        ),
        61 | 62 => faces("furnace_top", "furnace_side", "furnace_top"),
        64 => one("door_wood_lower"),
        65 => one("ladder"),
        66 => one("rail_normal"),
        67 => one("cobblestone"),
        71 => one("door_iron_lower"),
        73 | 74 => one("redstone_ore"),
        75 => one("redstone_torch_off"),
        76 => one("redstone_torch_on"),
        78 | 80 => one("snow"),
        79 | 174 | 212 => one(if id == 174 {
            "ice_packed"
        } else if id == 212 {
            "frosted_ice_0"
        } else {
            "ice"
        }),
        81 => faces("cactus_top", "cactus_side", "cactus_bottom"),
        82 => one("clay"),
        83 => one("reeds"),
        84 => faces("jukebox_top", "jukebox_side", "jukebox_side"),
        86 | 91 => faces("pumpkin_top", "pumpkin_side", "pumpkin_top"),
        87 => one("netherrack"),
        88 => one("soul_sand"),
        89 => one("glowstone"),
        90 => one("portal"),
        95 | 160 => one(&format!("glass_{}", colors[usize::from(d)])),
        96 => one("trapdoor"),
        97 => one("stone"),
        98 | 109 => one(match d {
            1 => "stonebrick_mossy",
            2 => "stonebrick_cracked",
            3 => "stonebrick_carved",
            _ => "stonebrick",
        }),
        99 => one("mushroom_block_skin_brown"),
        100 => one("mushroom_block_skin_red"),
        101 => one("iron_bars"),
        102 => one("glass"),
        103 => faces("melon_top", "melon_side", "melon_top"),
        104 => one("pumpkin_stem_disconnected"),
        105 => one("melon_stem_disconnected"),
        106 => one("vine"),
        110 => faces("mycelium_top", "mycelium_side", "dirt"),
        111 => one("waterlily"),
        112..=114 => one("nether_brick"),
        115 => one(&format!("nether_wart_stage{}", (d / 2).min(2))),
        116 => faces(
            "enchanting_table_top",
            "enchanting_table_side",
            "enchanting_table_bottom",
        ),
        121 => one("end_stone"),
        122 => one("dragon_egg"),
        123 | 124 => one(if id == 123 {
            "redstone_lamp_off"
        } else {
            "redstone_lamp_on"
        }),
        125 | 126 => one(&format!("planks_{}", wood[usize::from((d & 7).min(5))])),
        128 => one("sandstone_normal"),
        129 => one("emerald_ore"),
        133 => one("emerald_block"),
        134..=136 => one(&format!("planks_{}", wood[usize::from(id - 133)])),
        137 | 210 | 211 => one("command_block_front"),
        138 => one("beacon"),
        139 => one(if d == 1 {
            "cobblestone_mossy"
        } else {
            "cobblestone"
        }),
        141 => one(&format!("carrots_stage{}", (d / 2).min(3))),
        142 => one(&format!("potatoes_stage{}", (d / 2).min(3))),
        145 => one("anvil_base"),
        152 => one("redstone_block"),
        153 => one("quartz_ore"),
        155 | 156 => faces(
            "quartz_block_top",
            "quartz_block_side",
            "quartz_block_bottom",
        ),
        159 => one(&format!("hardened_clay_stained_{}", colors[usize::from(d)])),
        163 => one("planks_acacia"),
        164 => one("planks_big_oak"),
        165 => one("slime"),
        167 => one("iron_trapdoor"),
        168 => one(match d {
            1 => "prismarine_bricks",
            2 => "prismarine_dark",
            _ => "prismarine_rough",
        }),
        169 => one("sea_lantern"),
        170 => faces("hay_block_top", "hay_block_side", "hay_block_top"),
        171 => one(&format!("wool_colored_{}", colors[usize::from(d)])),
        172 => one("hardened_clay"),
        173 => one("coal_block"),
        175 => one([
            "double_plant_sunflower_bottom",
            "double_plant_syringa_bottom",
            "double_plant_grass_bottom",
            "double_plant_fern_bottom",
            "double_plant_rose_bottom",
            "double_plant_paeonia_bottom",
        ][usize::from((d & 7).min(5))]),
        179..=182 => faces(
            "red_sandstone_top",
            "red_sandstone_normal",
            "red_sandstone_bottom",
        ),
        183..=187 => one(&format!("planks_{}", wood[usize::from(id - 182)])),
        188..=192 => one(&format!("planks_{}", wood[usize::from(id - 187)])),
        198 => one("end_rod"),
        199 => one("chorus_plant"),
        200 => one("chorus_flower"),
        201 | 203 | 204 | 205 => one("purpur_block"),
        202 => faces("purpur_pillar_top", "purpur_pillar", "purpur_pillar_top"),
        206 => one("end_bricks"),
        207 => one(&format!("beetroots_stage{}", d.min(3))),
        208 => faces("grass_path_top", "grass_path_side", "dirt"),
        213 => one("magma"),
        214 => one("nether_wart_block"),
        215 => one("red_nether_brick"),
        216 => faces("bone_block_top", "bone_block_side", "bone_block_top"),
        218 => one("observer_side"),
        219..=234 => one(&format!("shulker_top_{}", colors[usize::from(id - 219)])),
        235..=250 => one(&format!(
            "glazed_terracotta_{}",
            colors[usize::from(id - 235)]
        )),
        251 => one(&format!("concrete_{}", colors[usize::from(d)])),
        252 => one(&format!("concrete_powder_{}", colors[usize::from(d)])),
        255 => one("structure_block"),
        _ => one("missing"),
    }
}

fn directional_names(id: u16, d: u8, mut top: String, side: &str) -> (String, [String; 4]) {
    let mut sides = std::array::from_fn(|_| side.to_owned());
    let facing = match d & 7 {
        2 => Some(2),
        3 => Some(3),
        4 => Some(0),
        5 => Some(1),
        _ => None,
    };
    match id {
        29 | 33 | 34 => {
            let head = if id == 29 || (id == 34 && d & 8 != 0) {
                "piston_top_sticky"
            } else {
                "piston_top_normal"
            };
            top = if d & 7 == 1 {
                head
            } else if d & 7 == 0 {
                "piston_bottom"
            } else {
                "piston_side"
            }
            .into();
            if let Some(f) = facing {
                sides[f] = head.into();
                sides[f ^ 1] = "piston_bottom".into();
            }
        }
        61 | 62 => {
            if let Some(f) = facing {
                sides[f] = if id == 62 {
                    "furnace_front_on"
                } else {
                    "furnace_front_off"
                }
                .into();
            }
        }
        23 | 158 => {
            let prefix = if id == 23 { "dispenser" } else { "dropper" };
            // In vanilla both use furnace_side; no dispenser_side file exists.
            sides = std::array::from_fn(|_| "furnace_side".into());
            if let Some(f) = facing {
                sides[f] = format!("{prefix}_front_horizontal");
            } else if d & 7 == 1 {
                top = format!("{prefix}_front_vertical");
            }
        }
        86 | 91 => {
            sides[[3, 0, 2, 1][usize::from(d & 3)]] = if id == 91 {
                "pumpkin_face_on"
            } else {
                "pumpkin_face_off"
            }
            .into();
        }
        17 | 162 | 170 | 202 | 216 => {
            let axis = d & 12;
            let end = top.clone();
            if axis == 4 {
                top = side.into();
                sides[0] = end.clone();
                sides[1] = end.clone();
            }
            if axis == 8 {
                top = side.into();
                sides[2] = end.clone();
                sides[3] = end;
            }
            if axis == 12 {
                top = side.into();
            }
        }
        218 => {
            top = "observer_top".into();
            if let Some(f) = facing {
                sides[f] = "observer_front".into();
                sides[f ^ 1] = "observer_back".into();
            } else {
                top = if d & 7 == 1 {
                    "observer_front"
                } else {
                    "observer_back"
                }
                .into();
            }
        }
        _ => {}
    }
    (top, sides)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn technical_materials_resolve_every_metadata_variant() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("blocks")).unwrap();
        for name in [
            "piston_top_sticky",
            "piston_top_normal",
            "piston_side",
            "piston_bottom",
            "lever",
            "stone",
            "repeater_off",
            "repeater_on",
            "stone_slab_side",
            "stone_slab_top",
            "trip_wire_source",
            "trip_wire",
            "planks_oak",
            "redstone_dust_line0",
            "comparator_off",
            "comparator_on",
        ] {
            RgbaImage::from_pixel(16, 16, image::Rgba([128, 128, 128, 255]))
                .save(dir.path().join("blocks").join(format!("{name}.png")))
                .unwrap();
        }
        let assets = Assets::load(dir.path()).unwrap();
        for id in [29, 33, 34, 55, 69, 77, 93, 94, 131, 132, 143, 149, 150] {
            for d in 0..16 {
                assert!(
                    !assets.unknown[id * 16 + d],
                    "unresolved technical block {id}:{d}"
                );
            }
        }
    }
    #[test]
    fn chest_entity_atlas_is_loaded_and_hashed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("blocks")).unwrap();
        RgbaImage::from_pixel(16, 16, image::Rgba([1, 2, 3, 255]))
            .save(dir.path().join("blocks/stone.png"))
            .unwrap();
        std::fs::create_dir_all(dir.path().join("entity/chest")).unwrap();
        let path = dir.path().join("entity/chest/normal.png");
        RgbaImage::from_pixel(64, 64, image::Rgba([180, 100, 40, 255]))
            .save(&path)
            .unwrap();
        let a = Assets::load(dir.path()).unwrap();
        assert!(!a.unknown[54 * 16]);
        assert_eq!(
            a.texels(a.material(54, 2).top)[8 * 16 + 8],
            [180, 100, 40, 255]
        );
        RgbaImage::from_pixel(64, 64, image::Rgba([40, 100, 180, 255]))
            .save(&path)
            .unwrap();
        let b = Assets::load(dir.path()).unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
    }
    #[test]
    fn legacy_end_frame_and_entity_names() {
        assert_eq!(texture_names(120, 0).0, "endframe_top");
        assert_eq!(texture_names(54, 0).0, "chest_top");
        assert_eq!(texture_names(193, 8).0, "door_spruce_upper");
    }
    #[test]
    fn technical_blocks_have_vanilla_texture_names() {
        for (id, data, name) in [
            (34, 12, "piston_top_sticky"),
            (69, 9, "lever"),
            (77, 4, "stone"),
            (93, 3, "repeater_off"),
            (94, 0, "repeater_on"),
            (131, 5, "trip_wire_source"),
            (132, 6, "trip_wire"),
            (55, 15, "redstone_dust_line0"),
            (149, 0, "comparator_off"),
            (150, 0, "comparator_on"),
        ] {
            assert_eq!(texture_names(id, data).0, name, "id {id}");
            assert_eq!(shape(id), Shape::Thin, "id {id}");
        }
    }
    #[test]
    fn directional_faces_use_world_normals() {
        for (metadata, face) in [(2, 2), (3, 3), (4, 0), (5, 1)] {
            let (top, side, _) = texture_names(61, metadata);
            let (_, sides) = directional_names(61, metadata, top, &side);
            assert_eq!(sides[face], "furnace_front_off");
            assert_eq!(
                sides
                    .iter()
                    .filter(|s| s.as_str() == "furnace_front_off")
                    .count(),
                1
            );
        }
        let (top, sides) = directional_names(17, 4, "log_oak_top".into(), "log_oak");
        assert_eq!(top, "log_oak");
        assert_eq!(sides[0], "log_oak_top");
        assert_eq!(sides[2], "log_oak");
    }
}
