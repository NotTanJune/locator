//! Image preview drawn with half-block characters.
//!
//! Each terminal cell shows two pixels, one above the other: the character is `▀` or `▄`,
//! and the foreground and background colours are the two pixels. This works in every
//! terminal that has 24-bit colour, and needs no graphics protocol.
//!
//! The sizing and the encoding follow the half-block path of `ratatui-image` 11, which this
//! module replaced, so a preview looks the same as it did with that crate.

use image::imageops::{self, FilterType};
use image::{DynamicImage, ImageBuffer, Rgba};
use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::style::Color;

/// Pixels that one cell stands for when an image is fitted to an area. A cell is taken to be
/// twice as tall as it is wide.
const CELL_WIDTH: u32 = 10;
const CELL_HEIGHT: u32 = 20;

const HALF_UPPER: char = '\u{2580}';
const HALF_LOWER: char = '\u{2584}';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HalfBlock {
    fg: Color,
    bg: Color,
    symbol: char,
}

/// An image and its encoding for the area it was last drawn in. The encoding is redone only
/// when the fitted size changes.
pub(crate) struct HalfblockImage {
    image: DynamicImage,
    size: Size,
    cells: Vec<HalfBlock>,
}

impl HalfblockImage {
    pub(crate) fn new(image: DynamicImage) -> Self {
        Self {
            image,
            size: Size::new(0, 0),
            cells: Vec::new(),
        }
    }

    /// Draws the image in the top left of `area`, scaled down to fit and never scaled up.
    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let size = fitted_size(&self.image, Size::new(area.width, area.height));
        if size != self.size || self.cells.is_empty() {
            self.cells = encode(&self.image, size);
            self.size = size;
        }
        let width = usize::from(self.size.width);
        if width == 0 {
            return;
        }
        for (index, block) in self.cells.iter().enumerate() {
            let x = (index % width) as u16;
            let y = (index / width) as u16;
            if x >= area.width || y >= area.height {
                continue;
            }
            if let Some(cell) = buf.cell_mut((area.x + x, area.y + y)) {
                cell.set_fg(block.fg)
                    .set_bg(block.bg)
                    .set_char(block.symbol);
            }
        }
    }
}

/// The cells the image covers inside `available`, keeping its proportions.
fn fitted_size(image: &DynamicImage, available: Size) -> Size {
    if image.width() == 0 || image.height() == 0 {
        return Size::new(0, 0);
    }
    let max_width = (u32::from(available.width) * CELL_WIDTH).min(image.width());
    let max_height = (u32::from(available.height) * CELL_HEIGHT).min(image.height());
    let ratio = f64::min(
        f64::from(max_width) / f64::from(image.width()),
        f64::from(max_height) / f64::from(image.height()),
    );
    let width = ((f64::from(image.width()) * ratio).round() as u32).max(1);
    let height = ((f64::from(image.height()) * ratio).round() as u32).max(1);
    Size::new(
        (width as f32 / CELL_WIDTH as f32).ceil() as u16,
        (height as f32 / CELL_HEIGHT as f32).ceil() as u16,
    )
}

/// One half block per cell of `size`, row by row.
fn encode(image: &DynamicImage, size: Size) -> Vec<HalfBlock> {
    let columns = u32::from(size.width);
    let rows = u32::from(size.height);
    if columns == 0 || rows == 0 || image.width() == 0 || image.height() == 0 {
        return Vec::new();
    }
    // Fit the image to the pixel box of the cells. A box the image does not fill exactly is
    // padded with transparent pixels on the right or at the bottom.
    let box_width = columns * CELL_WIDTH;
    let box_height = rows * CELL_HEIGHT;
    let mut fitted = image.resize(box_width, box_height, FilterType::Nearest);
    if fitted.width() != box_width || fitted.height() != box_height {
        let mut padded: DynamicImage =
            ImageBuffer::from_pixel(box_width, box_height, Rgba([0u8, 0, 0, 0])).into();
        imageops::overlay(&mut padded, &fitted, 0, 0);
        fitted = padded;
    }
    // Two pixels per cell: one column wide, two rows tall.
    let pixels = fitted
        .resize_exact(columns, rows * 2, FilterType::Triangle)
        .to_rgb8();

    let black = Color::Rgb(0, 0, 0);
    let mut cells = vec![
        HalfBlock {
            fg: black,
            bg: black,
            symbol: HALF_UPPER,
        };
        (columns * rows) as usize
    ];
    for (y, row) in pixels.rows().enumerate() {
        for (x, pixel) in row.enumerate() {
            let cell = &mut cells[x + columns as usize * (y / 2)];
            let color = Color::Rgb(pixel[0], pixel[1], pixel[2]);
            if y % 2 == 0 {
                cell.fg = color;
            } else {
                cell.bg = color;
            }
        }
    }
    for cell in &mut cells {
        if cell.fg == cell.bg {
            cell.symbol = ' ';
        } else if luminance(cell.bg) > luminance(cell.fg) {
            // Keep the brighter pixel in the foreground, which terminals draw more reliably.
            std::mem::swap(&mut cell.fg, &mut cell.bg);
            cell.symbol = HALF_LOWER;
        }
    }
    cells
}

fn luminance(color: Color) -> u32 {
    match color {
        Color::Rgb(r, g, b) => 2126 * u32::from(r) + 7152 * u32::from(g) + 722 * u32::from(b),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage, RgbaImage};

    fn sample(width: u32, height: u32, alpha: bool) -> DynamicImage {
        let mut state = 0x9e37_79b9_u32 ^ (width * 31 + height);
        let image = RgbaImage::from_fn(width, height, |x, y| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = (state >> 24) as u8;
            let a = if alpha {
                ((x * 7 + y * 13) % 256) as u8
            } else {
                255
            };
            Rgba([
                ((x * 255) / width.max(1)) as u8 ^ (noise & 0x1f),
                ((y * 255) / height.max(1)) as u8,
                noise,
                a,
            ])
        });
        DynamicImage::ImageRgba8(image)
    }

    /// The digest below was recorded while `ratatui-image` 11.0.6 was still a dependency and
    /// a test compared every one of these cells with its half-block output: 384 renders,
    /// 758,640 cells, no difference. A change to the digest means previews no longer look
    /// the way they did with that crate.
    #[test]
    fn output_is_the_same_as_ratatui_image_gave() {
        let images = [
            (1, 1),
            (3, 5),
            (10, 20),
            (11, 21),
            (37, 91),
            (640, 480),
            (480, 640),
            (2000, 300),
            (300, 2000),
            (1920, 1080),
            (17, 400),
            (400, 17),
        ];
        let areas = [
            (1, 1),
            (2, 7),
            (5, 3),
            (40, 20),
            (93, 37),
            (120, 50),
            (200, 9),
            (9, 60),
        ];
        let mut renders = 0usize;
        let mut cells = 0usize;
        let mut digest = 0xcbf2_9ce4_8422_2325_u64;
        for (width, height) in images {
            for alpha in [false, true] {
                let image = sample(width, height, alpha);
                for (columns, rows) in areas {
                    // Draw away from the origin, in a buffer larger than the area, and then
                    // again in a smaller area, which must fit the image again.
                    let full = Rect::new(0, 0, columns + 6, rows + 4);
                    let mut widget = HalfblockImage::new(image.clone());
                    for area in [
                        Rect::new(3, 2, columns, rows),
                        Rect::new(3, 2, columns.div_ceil(2), rows.div_ceil(2)),
                    ] {
                        let mut buffer = Buffer::empty(full);
                        widget.render(area, &mut buffer);
                        for y in 0..full.height {
                            for x in 0..full.width {
                                let cell = &buffer[(x, y)];
                                let text = format!("{}{:?}{:?}", cell.symbol(), cell.fg, cell.bg);
                                for byte in text.bytes() {
                                    digest =
                                        (digest ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
                                }
                                cells += 1;
                            }
                        }
                        renders += 1;
                    }
                }
            }
        }
        assert_eq!((renders, cells), (384, 758_640));
        assert_eq!(digest, 0x2280_ee09_d136_1d59, "digest {digest:#018x}");
    }

    #[test]
    fn a_cell_holds_the_upper_and_lower_pixel() {
        // One column, three rows of cells: red over blue, then grey to the bottom.
        let mut pixels = RgbImage::new(10, 60);
        for (_, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = match y {
                0..=9 => Rgb([255, 0, 0]),
                10..=19 => Rgb([0, 0, 255]),
                _ => Rgb([90, 90, 90]),
            };
        }
        let mut widget = HalfblockImage::new(DynamicImage::ImageRgb8(pixels));
        let area = Rect::new(0, 0, 4, 4);
        let mut buffer = Buffer::empty(area);
        widget.render(area, &mut buffer);

        let top = &buffer[(0, 0)];
        assert_eq!(top.symbol(), "\u{2580}");
        // The filter blends neighbouring rows a little, so check the dominant channel.
        let (Color::Rgb(upper_red, _, upper_blue), Color::Rgb(lower_red, _, lower_blue)) =
            (top.fg, top.bg)
        else {
            panic!("expected RGB colours, got {:?} and {:?}", top.fg, top.bg);
        };
        assert!(upper_red > 200 && upper_blue < 60, "upper {:?}", top.fg);
        assert!(lower_blue > 200 && lower_red < 60, "lower {:?}", top.bg);
        // Equal halves need no glyph. The last cell is far enough from the blue to be pure grey.
        let flat = &buffer[(0, 2)];
        assert_eq!(flat.symbol(), " ");
        assert_eq!(flat.bg, Color::Rgb(90, 90, 90));
        // The image is one cell wide and three tall. Nothing else is drawn.
        assert_eq!(buffer[(1, 0)], ratatui::buffer::Cell::default());
        assert_eq!(buffer[(0, 3)], ratatui::buffer::Cell::default());
    }

    #[test]
    fn the_brighter_pixel_goes_to_the_foreground() {
        let mut pixels = RgbImage::new(10, 20);
        for (_, y, pixel) in pixels.enumerate_pixels_mut() {
            *pixel = if y < 10 {
                Rgb([10, 10, 10])
            } else {
                Rgb([240, 240, 240])
            };
        }
        let mut widget = HalfblockImage::new(DynamicImage::ImageRgb8(pixels));
        let area = Rect::new(0, 0, 1, 1);
        let mut buffer = Buffer::empty(area);
        widget.render(area, &mut buffer);
        let cell = &buffer[(0, 0)];
        assert_eq!(cell.symbol(), "\u{2584}");
        assert!(
            luminance(cell.fg) > luminance(cell.bg),
            "{:?} over {:?}",
            cell.fg,
            cell.bg
        );
    }

    #[test]
    fn an_image_is_fitted_to_the_area_and_never_enlarged() {
        // 640x480 pixels is 64x24 cells at its own size.
        let image = sample(640, 480, false);
        assert_eq!(fitted_size(&image, Size::new(200, 100)), Size::new(64, 24));
        // A smaller area scales it down and keeps its proportions.
        assert_eq!(fitted_size(&image, Size::new(40, 100)), Size::new(40, 15));
        assert_eq!(fitted_size(&image, Size::new(200, 6)), Size::new(16, 6));
    }

    #[test]
    fn an_empty_area_or_image_draws_nothing() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 4));
        let untouched = buffer.clone();
        HalfblockImage::new(sample(20, 20, false)).render(Rect::new(0, 0, 0, 4), &mut buffer);
        HalfblockImage::new(DynamicImage::new_rgb8(0, 0))
            .render(Rect::new(0, 0, 4, 4), &mut buffer);
        assert_eq!(buffer, untouched);
    }
}
