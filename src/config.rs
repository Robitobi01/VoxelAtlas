use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub output: PathBuf,
    pub textures: PathBuf,
    #[serde(default = "default_workers")]
    pub workers: usize,
    #[serde(default = "default_cache")]
    pub cache_mib: usize,
    #[serde(default = "default_tile")]
    pub tile_size: u32,
    #[serde(default)]
    pub encoding: Encoding,
    /// Skip damaged chunk data with warnings; source changes and I/O errors remain fatal.
    #[serde(default)]
    pub skip_corrupt_chunks: bool,
    pub worlds: BTreeMap<String, WorldConfig>,
    pub maps: Vec<MapConfig>,
}
/// `Fast` and `Compact` preserve every visible RGB value and alpha exactly and
/// differ only in encoder effort. `Indexed` additionally reduces each tile to a
/// 256-colour adapted palette before encoding, which roughly halves isometric
/// storage; that is a visible change, and it is the same kind of reduction
/// Mapcrafter's `png_indexed` option applies.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    #[default]
    Fast,
    Compact,
    Indexed,
}
impl Encoding {
    /// Whether the profile preserves every rendered pixel exactly.
    pub fn is_exact(self) -> bool {
        !matches!(self, Self::Indexed)
    }
}
fn default_workers() -> usize {
    4
}
fn default_cache() -> usize {
    128
}
fn default_tile() -> u32 {
    512
}
fn default_water() -> f32 {
    0.8
}
fn default_water_light() -> f32 {
    0.5
}
fn default_rotations() -> Vec<u8> {
    vec![0]
}
fn default_zoom() -> u8 {
    7
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorldConfig {
    pub path: PathBuf,
    #[serde(default)]
    pub dimension: Dimension,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Dimension {
    #[default]
    Overworld,
    Nether,
    End,
}
impl Dimension {
    pub fn directory(self, root: &Path) -> PathBuf {
        match self {
            Self::Overworld => root.join("region"),
            Self::Nether => root.join("DIM-1/region"),
            Self::End => root.join("DIM1/region"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MapConfig {
    pub id: String,
    pub world: String,
    #[serde(default)]
    pub view: View,
    #[serde(default = "default_rotations")]
    pub rotations: Vec<u8>,
    #[serde(default)]
    pub mode: Mode,
    #[serde(default = "default_water")]
    pub water_opacity: f32,
    #[serde(default = "default_water_light")]
    pub lighting_water_intensity: f32,
    #[serde(default = "default_zoom")]
    pub default_zoom: u8,
}
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum View {
    #[default]
    Isometric,
    Topdown,
}
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Daylight,
    Plain,
}

pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self = toml::from_str(&text).context("invalid configuration")?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        if self.workers == 0 || self.workers > 256 {
            bail!("workers must be between 1 and 256");
        }
        if self.cache_mib < 16 || self.cache_mib > 65536 {
            bail!("cache_mib must be between 16 and 65536");
        }
        if !matches!(self.tile_size, 256 | 512) {
            bail!("tile_size must be 256 or 512");
        }
        if self.maps.is_empty() {
            bail!("at least one map is required");
        }
        let mut ids = std::collections::BTreeSet::new();
        for (id, w) in &self.worlds {
            if !valid_id(id) {
                bail!("invalid world id: {id}");
            }
            for mask in &w.exclude {
                parse_exclusion(mask)?;
            }
        }
        for map in &self.maps {
            if !valid_id(&map.id) || !ids.insert(&map.id) {
                bail!("invalid or duplicate map id: {}", map.id);
            }
            if !self.worlds.contains_key(&map.world) {
                bail!("unknown world: {}", map.world);
            }
            if map.rotations.is_empty()
                || map.rotations.iter().any(|r| *r > 3)
                || map
                    .rotations
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != map.rotations.len()
            {
                bail!("rotations must be a nonempty unique list of 0..3");
            }
            if map.view == View::Topdown && map.rotations != [0] {
                bail!("topdown supports rotation 0 only");
            }
            if !map.water_opacity.is_finite()
                || !(0.0..=1.0).contains(&map.water_opacity)
                || !map.lighting_water_intensity.is_finite()
                || !(0.0..=1.0).contains(&map.lighting_water_intensity)
            {
                bail!("water settings must be between 0 and 1");
            }
        }
        Ok(())
    }
}

pub fn parse_exclusion(value: &str) -> Result<(u16, Option<u8>)> {
    let (id, data) = value
        .split_once(':')
        .map_or((value, None), |(a, b)| (a, Some(b)));
    let id: u16 = id
        .parse()
        .context("block exclusion must be ID or ID:metadata")?;
    if id > 4095 {
        bail!("block ID exceeds 4095");
    }
    let data = data
        .map(str::parse::<u8>)
        .transpose()
        .context("invalid block metadata")?;
    if data.is_some_and(|d| d > 15) {
        bail!("metadata exceeds 15");
    }
    Ok((id, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn masks() {
        assert_eq!(parse_exclusion("43:6").unwrap(), (43, Some(6)));
        assert!(parse_exclusion("44:16").is_err());
        assert!(parse_exclusion("4096").is_err());
    }
    #[test]
    fn ids() {
        assert!(valid_id("Survival-Overworld"));
        assert!(!valid_id("../escape"));
    }
}
