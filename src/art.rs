//! Image → terminal-art conversion, used wherever the player shows pictures
//! (artist tiles, release covers, now-playing).
//!
//! The format is "half-block art": each terminal cell renders `▀` with the
//! foreground colored as the top pixel and the background as the bottom
//! pixel, giving 2 vertical pixels per cell. This needs no terminal image
//! protocol (sixel/kitty), so it works everywhere crossterm does, and it
//! looks far better than glyph-luminance ASCII at tile sizes. The UI layer
//! converts `ArtImage` cells into styled spans.

use anyhow::{Context as _, Result};

/// Cache key for the shared artwork cache: the same image can be cached at
/// several cell sizes (grid tile vs page header).
pub fn cache_key(url: &str, width_cells: u16, height_cells: u16) -> String {
    format!("{url}#{width_cells}x{height_cells}")
}

/// Decoded, cell-sized art. `pixels` holds `width_cells * height_cells * 2`
/// RGB triples, row-major, two pixel rows per cell row (top, then bottom).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtImage {
    pub width_cells: u16,
    pub height_cells: u16,
    pixels: Vec<[u8; 3]>,
}

impl ArtImage {
    /// Top and bottom pixel of the cell at (column, cell row).
    pub fn cell(&self, x: u16, y: u16) -> ([u8; 3], [u8; 3]) {
        let width = self.width_cells as usize;
        let top = (y as usize * 2) * width + x as usize;
        let bottom = (y as usize * 2 + 1) * width + x as usize;
        (self.pixels[top], self.pixels[bottom])
    }
}

/// Decode image bytes (jpeg/png/webp/gif/bmp) and scale them to fill a
/// `width_cells` × `height_cells` terminal area, center-cropping overflow
/// like CSS object-fit: cover.
pub fn decode_to_cells(bytes: &[u8], width_cells: u16, height_cells: u16) -> Result<ArtImage> {
    let width_px = u32::from(width_cells.max(1));
    let height_px = u32::from(height_cells.max(1)) * 2;
    let image = image::load_from_memory(bytes).context("unsupported or corrupt image")?;
    let image = image
        .resize_to_fill(width_px, height_px, image::imageops::FilterType::Triangle)
        .into_rgb8();
    let pixels = image.pixels().map(|p| p.0).collect();
    Ok(ArtImage {
        width_cells: width_cells.max(1),
        height_cells: height_cells.max(1),
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_2x2() -> Vec<u8> {
        // red, green / blue, white
        let mut img = image::RgbImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgb([255, 0, 0]));
        img.put_pixel(1, 0, image::Rgb([0, 255, 0]));
        img.put_pixel(0, 1, image::Rgb([0, 0, 255]));
        img.put_pixel(1, 1, image::Rgb([255, 255, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn decodes_to_requested_cell_grid() {
        let art = decode_to_cells(&png_2x2(), 2, 1).unwrap();
        assert_eq!((art.width_cells, art.height_cells), (2, 1));
        let (top, bottom) = art.cell(0, 0);
        assert_eq!(top, [255, 0, 0]);
        assert_eq!(bottom, [0, 0, 255]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_to_cells(b"not an image", 4, 4).is_err());
    }
}
