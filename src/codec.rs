//! Bounded pixel deduplication and native WebP pyramid decoding.
use crate::output::atomic_write;
use anyhow::{Context, Result, bail};
use image::RgbaImage;
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

// This cache stores hashes only, never images. At most a few MiB even on huge maps.
const MAX_PIXEL_ENTRIES: usize = 32_768;
type Encoded = OnceLock<std::result::Result<String, String>>;

pub struct TileEncoder {
    output: PathBuf,
    pixels: Mutex<HashMap<blake3::Hash, Arc<Encoded>>>,
    quality: f32,
    method: i32,
    /// Palette size applied before encoding; `None` keeps every rendered pixel.
    palette: Option<usize>,
    pub encoded: AtomicU64,
    pub reused: AtomicU64,
}
impl TileEncoder {
    pub fn new(output: &Path) -> Self {
        Self {
            output: output.to_owned(),
            pixels: Mutex::new(HashMap::new()),
            quality: 60.0,
            method: 0,
            palette: None,
            encoded: AtomicU64::new(0),
            reused: AtomicU64::new(0),
        }
    }
    pub fn compact(output: &Path, base: bool) -> Self {
        Self {
            quality: 100.0,
            method: if base { 1 } else { 0 },
            ..Self::new(output)
        }
    }
    /// Reduce each tile to an adapted 256-colour palette before encoding.
    /// Deduplication still keys on the rendered pixels, and quantization is a
    /// pure function of them, so identical tiles still encode once.
    pub fn indexed(output: &Path) -> Self {
        Self {
            palette: Some(256),
            ..Self::new(output)
        }
    }
    pub fn encode(&self, image: &RgbaImage) -> Result<Option<String>> {
        if image.pixels().all(|p| p[3] == 0) {
            return Ok(None);
        }
        let mut hash = blake3::Hasher::new();
        hash.update(&image.width().to_le_bytes());
        hash.update(&image.height().to_le_bytes());
        hash.update(image.as_raw());
        let key = hash.finalize();
        let entry = {
            let mut pixels = self.pixels.lock().unwrap();
            if let Some(entry) = pixels.get(&key) {
                self.reused.fetch_add(1, Ordering::Relaxed);
                Some(entry.clone())
            } else if pixels.len() < MAX_PIXEL_ENTRIES {
                let entry = Arc::new(OnceLock::new());
                pixels.insert(key, entry.clone());
                Some(entry)
            } else {
                None
            }
        };
        let result = if let Some(entry) = entry {
            entry
                .get_or_init(|| self.encode_unique(image).map_err(|e| format!("{e:#}")))
                .clone()
                .map_err(anyhow::Error::msg)?
        } else {
            self.encode_unique(image)?
        };
        Ok(Some(result))
    }
    fn encode_unique(&self, image: &RgbaImage) -> Result<String> {
        self.encoded.fetch_add(1, Ordering::Relaxed);
        let mut reduced;
        let image = match self.palette {
            Some(limit) => {
                reduced = image.clone();
                crate::quantize::quantize(&mut reduced, limit);
                &reduced
            }
            None => image,
        };
        let encoder = webp::Encoder::from_rgba(image.as_raw(), image.width(), image.height());
        let mut config =
            webp::WebPConfig::new().map_err(|_| anyhow::anyhow!("initializing WebP encoder"))?;
        config.lossless = 1;
        config.quality = self.quality;
        config.method = self.method;
        config.thread_level = 0;
        config.exact = 1;
        let bytes = encoder
            .encode_advanced(&config)
            .map_err(|e| anyhow::anyhow!("WebP encoding failed: {e:?}"))?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        let path = self.output.join("tiles").join(format!("{hash}.webp"));
        if !path.exists() {
            atomic_write(&path, &bytes)?;
        }
        Ok(hash)
    }
}

/// Decode directly to a half-resolution tile, avoiding an intermediate RGBA copy.
/// Full opacity uses exact integer averaging; partial alpha matches output::reduce.
pub fn decode_half(bytes: &[u8], size: u32) -> Result<RgbaImage> {
    let features = webp::BitstreamFeatures::new(bytes).context("invalid cached WebP tile")?;
    if features.width() != size
        || features.height() != size
        || features.has_animation()
        || size == 0
        || !size.is_multiple_of(2)
    {
        bail!("cached tile has incorrect dimensions or format");
    }
    let decoded = webp::Decoder::new(bytes)
        .decode()
        .context("decoding cached WebP tile")?;
    let channels = if features.has_alpha() { 4 } else { 3 };
    let mut half = RgbaImage::new(size / 2, size / 2);
    let raw: &[u8] = &decoded;
    let stride = size as usize * channels;
    for (y, row) in half
        .as_mut()
        .chunks_exact_mut((size / 2) as usize * 4)
        .enumerate()
    {
        let a = &raw[y * 2 * stride..(y * 2 + 1) * stride];
        let b = &raw[(y * 2 + 1) * stride..(y * 2 + 2) * stride];
        for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let offset = x * 2 * channels;
            let p = [
                &a[offset..offset + channels],
                &a[offset + channels..offset + channels * 2],
                &b[offset..offset + channels],
                &b[offset + channels..offset + channels * 2],
            ];
            let alpha = if channels == 3 {
                1020
            } else {
                p.iter().map(|p| u32::from(p[3])).sum()
            };
            if alpha == 1020 {
                for c in 0..3 {
                    pixel[c] = ((u32::from(p[0][c])
                        + u32::from(p[1][c])
                        + u32::from(p[2][c])
                        + u32::from(p[3][c]))
                        / 4) as u8;
                }
                pixel[3] = 255;
            } else if alpha > 0 {
                for c in 0..3 {
                    let total: u32 = p.iter().map(|p| u32::from(p[c]) * u32::from(p[3])).sum();
                    pixel[c] = total.checked_div(alpha).unwrap_or(0) as u8;
                }
                pixel[3] = ((alpha + 2) / 4) as u8;
            }
        }
    }
    Ok(half)
}

pub fn read_half(output: &Path, hash: &str, size: u32) -> Result<RgbaImage> {
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid tile hash");
    }
    decode_half(
        &fs::read(output.join("tiles").join(format!("{hash}.webp")))?,
        size,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_reduction_matches_reference_for_rgb_and_alpha() {
        for alpha in [false, true] {
            let image = RgbaImage::from_fn(32, 32, |x, y| {
                image::Rgba([
                    (x * 7) as u8,
                    (y * 7) as u8,
                    ((x + y) * 3) as u8,
                    if alpha {
                        ((x * 13 + y * 9) % 256) as u8
                    } else {
                        255
                    },
                ])
            });
            let encoded = webp::Encoder::from_rgba(image.as_raw(), 32, 32).encode_lossless();
            assert_eq!(
                decode_half(&encoded, 32).unwrap(),
                crate::output::reduce(&image)
            );
        }
    }
    #[test]
    fn deduplicates_parallel_identical_pixels_before_encoding() {
        let dir = tempfile::tempdir().unwrap();
        let encoder = TileEncoder::new(dir.path());
        let image = RgbaImage::from_pixel(32, 32, image::Rgba([1, 2, 3, 255]));
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let encoder = &encoder;
                let image = &image;
                scope.spawn(move || {
                    encoder.encode(image).unwrap();
                });
            }
        });
        assert_eq!(encoder.encoded.load(Ordering::Relaxed), 1);
        assert_eq!(encoder.reused.load(Ordering::Relaxed), 7);
    }
    #[test]
    fn rejects_wrong_dimensions_before_decode() {
        let image = RgbaImage::from_pixel(32, 32, image::Rgba([1, 2, 3, 255]));
        let encoded = webp::Encoder::from_rgba(image.as_raw(), 32, 32).encode_lossless();
        assert!(decode_half(&encoded, 512).is_err());
    }
}
