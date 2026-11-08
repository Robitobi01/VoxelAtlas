//! Deterministic median-cut colour reduction for tiles.
//!
//! Rendered tiles hold only ~1,300 distinct colours in 262,144 pixels, because
//! shading is driven by 16 discrete light levels and a bounded set of biome
//! tints. Merging those into an adapted 256-colour palette roughly halves the
//! encoded size: neighbouring pixels that differed only below the visible
//! threshold collapse onto one entry and lengthen the encoder's matches.
//!
//! This is the same kind of reduction Mapcrafter's `png_indexed` performs, and
//! it is only used by the opt-in `indexed` profile. The result depends solely
//! on the pixels, never on iteration order, thread count or floating point, so
//! content-addressed tiles stay reproducible.
use image::RgbaImage;

/// Power-of-two probe table. Tiles with more distinct colours than half of this
/// are left untouched rather than risking a pathological probe sequence.
const SLOTS: usize = 1 << 16;
const MAX_COLORS: usize = SLOTS / 2;

struct Scratch {
    /// Packed RGBA key per slot, meaningful only when `stamp` is current.
    keys: Vec<u32>,
    /// Index into the distinct-colour list, per slot.
    indexes: Vec<u32>,
    stamp: Vec<u32>,
    generation: u32,
    /// Distinct-colour index per pixel, so the rewrite pass needs no probing.
    pixels: Vec<u32>,
    colors: Vec<Color>,
}
#[derive(Clone, Copy)]
struct Color {
    value: [u8; 4],
    count: u32,
}
impl Scratch {
    fn new() -> Self {
        Self {
            keys: vec![0; SLOTS],
            indexes: vec![0; SLOTS],
            stamp: vec![0; SLOTS],
            generation: 0,
            pixels: Vec::new(),
            colors: Vec::new(),
        }
    }
}
thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::new());
}

fn pack(pixel: [u8; 4]) -> u32 {
    u32::from_le_bytes(pixel)
}

/// Reduce `image` to at most `limit` colours in place. Returns false when the
/// image already fits, or holds too many colours to index, leaving it unchanged.
pub fn quantize(image: &mut RgbaImage, limit: usize) -> bool {
    assert!(limit >= 2, "a palette needs at least two entries");
    SCRATCH.with_borrow_mut(|scratch| {
        let pixels = image.as_chunks_mut::<4>().0;
        scratch.generation = scratch.generation.wrapping_add(1);
        if scratch.generation == 0 {
            scratch.stamp.fill(0);
            scratch.generation = 1;
        }
        let generation = scratch.generation;
        scratch.colors.clear();
        scratch.pixels.clear();
        scratch.pixels.reserve(pixels.len());
        for &pixel in pixels.iter() {
            let key = pack(pixel);
            let mut slot = (key.wrapping_mul(0x9E37_79B1) >> 16) as usize % SLOTS;
            loop {
                if scratch.stamp[slot] != generation {
                    if scratch.colors.len() >= MAX_COLORS {
                        return false;
                    }
                    scratch.stamp[slot] = generation;
                    scratch.keys[slot] = key;
                    scratch.indexes[slot] = scratch.colors.len() as u32;
                    scratch.colors.push(Color {
                        value: pixel,
                        count: 1,
                    });
                    break;
                }
                if scratch.keys[slot] == key {
                    scratch.colors[scratch.indexes[slot] as usize].count += 1;
                    break;
                }
                slot = (slot + 1) % SLOTS;
            }
            scratch.pixels.push(scratch.indexes[slot]);
        }
        if scratch.colors.len() <= limit {
            return false;
        }
        // Median cut. `order` is partitioned in place so each box owns a range;
        // `replacement` maps a distinct colour to the average of its final box.
        let mut order: Vec<u32> = (0..scratch.colors.len() as u32).collect();
        order.sort_unstable_by_key(|&i| (scratch.colors[i as usize].value, i));
        let span = order.len();
        let mut boxes = vec![Region::new(&scratch.colors, &mut order, 0, span)];
        while boxes.len() < limit {
            let Some(index) = (0..boxes.len())
                .filter(|&i| boxes[i].end - boxes[i].start > 1)
                .max_by_key(|&i| (boxes[i].spread, boxes[i].count, boxes[i].start))
            else {
                break;
            };
            let region = boxes[index];
            let Some(split) = region.split(&scratch.colors, &order) else {
                // Cannot divide further; leave it whole and look elsewhere.
                boxes[index].spread = 0;
                continue;
            };
            boxes[index] = Region::new(&scratch.colors, &mut order, region.start, split);
            boxes.push(Region::new(&scratch.colors, &mut order, split, region.end));
        }
        let mut replacement = vec![[0u8; 4]; scratch.colors.len()];
        for region in &boxes {
            let value = region.average(&scratch.colors, &order);
            for &i in &order[region.start..region.end] {
                replacement[i as usize] = value;
            }
        }
        for (pixel, &index) in pixels.iter_mut().zip(&scratch.pixels) {
            *pixel = replacement[index as usize];
        }
        true
    })
}

#[derive(Clone, Copy)]
struct Region {
    start: usize,
    end: usize,
    spread: u32,
    count: u32,
}
impl Region {
    fn new(colors: &[Color], order: &mut [u32], start: usize, end: usize) -> Self {
        let mut spread = 0;
        let mut channel = 0;
        let mut count = 0;
        for &i in &order[start..end] {
            count += colors[i as usize].count;
        }
        for c in 0..4 {
            let mut low = 255u8;
            let mut high = 0u8;
            for &i in &order[start..end] {
                low = low.min(colors[i as usize].value[c]);
                high = high.max(colors[i as usize].value[c]);
            }
            // Alpha errors show as haloes along tile edges, so weight them up.
            let range = u32::from(high - low) * if c == 3 { 4 } else { 1 };
            if range > spread {
                spread = range;
                channel = c;
            }
        }
        // Order the range on the channel it will be cut along, so splitting is
        // a single index choice rather than another sort.
        order[start..end].sort_unstable_by_key(|&i| {
            (
                colors[i as usize].value[channel],
                colors[i as usize].value,
                i,
            )
        });
        Self {
            start,
            end,
            spread,
            count,
        }
    }
    /// Index that puts half the pixel weight on each side, never degenerate.
    fn split(&self, colors: &[Color], order: &[u32]) -> Option<usize> {
        if self.spread == 0 {
            return None;
        }
        let mut running = 0u64;
        let total = u64::from(self.count);
        for (offset, &i) in order[self.start..self.end].iter().enumerate() {
            running += u64::from(colors[i as usize].count);
            if running * 2 >= total {
                let split = self.start + offset + 1;
                return Some(split.clamp(self.start + 1, self.end - 1));
            }
        }
        None
    }
    fn average(&self, colors: &[Color], order: &[u32]) -> [u8; 4] {
        let total: u64 = order[self.start..self.end]
            .iter()
            .map(|&i| u64::from(colors[i as usize].count))
            .sum();
        std::array::from_fn(|c| {
            let sum: u64 = order[self.start..self.end]
                .iter()
                .map(|&i| {
                    u64::from(colors[i as usize].value[c]) * u64::from(colors[i as usize].count)
                })
                .sum();
            ((sum + total / 2) / total) as u8
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn unique(image: &RgbaImage) -> usize {
        image
            .as_chunks::<4>()
            .0
            .iter()
            .copied()
            .collect::<HashSet<_>>()
            .len()
    }

    #[test]
    fn leaves_images_that_already_fit_untouched() {
        let mut image = RgbaImage::from_fn(16, 16, |x, _| image::Rgba([(x * 16) as u8, 0, 0, 255]));
        let before = image.clone();
        assert!(!quantize(&mut image, 256));
        assert_eq!(image, before);
    }

    #[test]
    fn reduces_to_the_limit_and_is_order_independent() {
        let build = || {
            RgbaImage::from_fn(64, 64, |x, y| {
                image::Rgba([(x * 3) as u8, (y * 3) as u8, ((x + y) * 2) as u8, 255])
            })
        };
        let mut image = build();
        assert!(unique(&image) > 256);
        assert!(quantize(&mut image, 256));
        assert!(unique(&image) <= 256, "{} colours", unique(&image));
        // Re-running is idempotent, and a second independent image agrees.
        let mut again = build();
        assert!(quantize(&mut again, 256));
        assert_eq!(image, again);
        let repeated = image.clone();
        assert!(!quantize(&mut image, 256));
        assert_eq!(image, repeated);
    }

    #[test]
    fn preserves_fully_transparent_and_fully_opaque_separation() {
        let mut image = RgbaImage::from_fn(64, 64, |x, y| {
            if (x + y) % 2 == 0 {
                image::Rgba([0, 0, 0, 0])
            } else {
                image::Rgba([(x * 4) as u8, (y * 4) as u8, 200, 255])
            }
        });
        assert!(quantize(&mut image, 16));
        for (x, y, pixel) in image.enumerate_pixels() {
            if (x + y) % 2 == 0 {
                assert_eq!(pixel.0[3], 0, "transparent pixel gained alpha");
            } else {
                assert!(pixel.0[3] > 200, "opaque pixel lost alpha");
            }
        }
    }
}
