//! Image sizing for the vision encoder, ported from `ImageSizing.targetSize` in DigUp
//! (`Sources/LlamaRuntime/ImageSizing.swift`, MIT; see tests/fixtures/digup-reference/NOTICE.md).
//! The reference fills the vision token budget, so images are resized here before mtmd sees them.

use std::path::Path;

use anyhow::{Context, Result};

/// Each soft token is a 3 x 3 block of 16 px patches; the encoder works in 48 px steps.
const PATCH: usize = 16;
const POOLING: usize = 3;
const SIDE: usize = POOLING * PATCH;

/// Decodes an image, resizes it to fill `budget` soft tokens (`target_size`) with a CatmullRom
/// filter, and returns (width, height, packed RGB8).
pub fn resize_for_budget(path: &Path, budget: u32) -> Result<(u32, u32, Vec<u8>)> {
    let decoded = image::ImageReader::open(path)
        .with_context(|| format!("open {}", path.display()))?
        .with_guessed_format()
        .context("guess image format")?
        .decode()
        .context("decode image")?
        .to_rgb8();
    let (width, height) = decoded.dimensions();
    let (target_height, target_width) =
        target_size(height as usize, width as usize, budget as usize)
            .context("image is too small to size for the vision budget")?;
    let resized = image::imageops::resize(
        &decoded,
        target_width as u32,
        target_height as u32,
        image::imageops::FilterType::CatmullRom,
    );
    Ok((
        target_width as u32,
        target_height as u32,
        resized.into_raw(),
    ))
}

/// Size (height, width) an image is scaled to so it fills `budget` soft tokens, keeping its aspect
/// ratio, with each side a multiple of 48 px. `None` when the image is too degenerate to size.
pub fn target_size(height: usize, width: usize, budget: usize) -> Option<(usize, usize)> {
    if height == 0 || width == 0 {
        return None;
    }
    let max_patches = budget * POOLING * POOLING;
    let factor = ((max_patches * PATCH * PATCH) as f64 / (height * width) as f64).sqrt();
    let mut target_height = (factor * height as f64 / SIDE as f64).floor() as usize * SIDE;
    let mut target_width = (factor * width as f64 / SIDE as f64).floor() as usize * SIDE;
    if target_height == 0 && target_width == 0 {
        return None;
    }
    let max_side = (max_patches / (POOLING * POOLING)) * SIDE;
    if target_height == 0 {
        target_height = SIDE;
        target_width = (width / height * SIDE).min(max_side);
    } else if target_width == 0 {
        target_width = SIDE;
        target_height = (height / width * SIDE).min(max_side);
    }
    Some((target_height, target_width))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values are DigUp's targetSize output, from a standalone compile of the Swift
    // function (height x width in the comments; the function takes height first).
    #[test]
    fn landscape_photo_fills_the_budget() {
        // 4000 x 3000 at 140 -> 480 x 624 (h x w)
        assert_eq!(target_size(3000, 4000, 140), Some((480, 624)));
    }

    #[test]
    fn photo_from_the_reference_set_at_140() {
        // 1024 x 768 at 140 -> 480 x 624
        assert_eq!(target_size(768, 1024, 140), Some((480, 624)));
    }

    #[test]
    fn screenshot_at_280() {
        // 1440 x 900 at 280 -> 624 x 1008
        assert_eq!(target_size(900, 1440, 280), Some((624, 1008)));
    }

    #[test]
    fn portrait_keeps_the_aspect_ratio() {
        // 3000 x 4000 (h x w) at 140 -> 624 x 480
        assert_eq!(target_size(4000, 3000, 140), Some((624, 480)));
    }

    #[test]
    fn square_at_70() {
        assert_eq!(target_size(1000, 1000, 70), Some((384, 384)));
    }

    #[test]
    fn strip_with_a_usable_short_side() {
        // 100 x 4000 at 140 -> 48 x 3552
        assert_eq!(target_size(100, 4000, 140), Some((48, 3552)));
    }

    #[test]
    fn very_short_strip_takes_the_side_limit_branch() {
        // 10 x 4000 at 70: the short side rounds to 0, so it becomes 48 and the long side is
        // capped at budget * 48 = 3360.
        assert_eq!(target_size(10, 4000, 70), Some((48, 3360)));
    }

    #[test]
    fn wide_image_at_70_and_1120() {
        assert_eq!(target_size(511, 977, 70), Some((288, 528)));
        assert_eq!(target_size(511, 977, 1120), Some((1152, 2208)));
    }

    #[test]
    fn every_side_is_a_multiple_of_48() {
        for (h, w) in [(3000usize, 4000usize), (768, 1024), (900, 1440), (511, 977)] {
            for budget in [70usize, 140, 280, 560, 1120] {
                let (th, tw) = target_size(h, w, budget).expect("sized");
                assert_eq!(th % SIDE, 0, "{h}x{w} at {budget}");
                assert_eq!(tw % SIDE, 0, "{h}x{w} at {budget}");
            }
        }
    }

    #[test]
    fn empty_image_has_no_size() {
        assert_eq!(target_size(0, 100, 140), None);
        assert_eq!(target_size(100, 0, 140), None);
    }
}
