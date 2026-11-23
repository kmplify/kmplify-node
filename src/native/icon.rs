//! The KMPLIFY mark, for the window's side rail and as the window icon.
//!
//! `assets/kmplify-mark-256.alpha` is the mark of the web dashboard
//! (`src/dashboard/assets/index.html`, `.logo-mark`) rasterised once to a
//! 256 x 256 coverage mask, one byte per pixel (egui has no image decoder
//! compiled in, and a mask is a quarter of an RGBA file). Regenerate with:
//!
//! ```text
//! # the .logo-mark <svg> from index.html, fill="#ffffff", saved as mark.svg
//! rsvg-convert -w 256 -h 256 mark.svg -o mark.png
//! python3 -c "from PIL import Image; im=Image.open('mark.png').convert('RGBA'); \
//!   open('kmplify-mark-256.alpha','wb').write(bytes(p[3] for p in im.getdata()))"
//! ```
//!
//! The window icon is composed from the same mask the way the dashboard's
//! tab icon (`favicon.svg`) is drawn: the white mark on the dark rounded
//! tile. One source, so the rail, the icon and the web page cannot drift
//! apart. The KMPLIFY name and logo are trademarks and not licensed under
//! Apache-2.0 (TRADEMARKS.md).

use eframe::egui;

/// Side length of the mask.
pub const MARK_SIZE: usize = 256;
/// The mark's coverage, row by row, 0 (none) to 255 (full).
pub const MARK: &[u8] = include_bytes!("assets/kmplify-mark-256.alpha");

/// The tile behind the mark in the icon: the dashboard's dark `--bg`.
const TILE: [f32; 3] = [10.0, 16.0, 20.0];

/// The mark as a texture image: white with the mask as alpha, so it can be
/// tinted to the scheme's logo colour when drawn.
pub fn mark_image() -> egui::ColorImage {
    let pixels = MARK
        .iter()
        .map(|a| egui::Color32::from_rgba_unmultiplied(255, 255, 255, *a))
        .collect();
    egui::ColorImage::new([MARK_SIZE, MARK_SIZE], pixels)
}

/// Bilinear sample of the mask at (u, v) in mask pixels; 0 outside.
fn coverage(u: f32, v: f32) -> f32 {
    let at = |x: i32, y: i32| -> f32 {
        if x < 0 || y < 0 || x >= MARK_SIZE as i32 || y >= MARK_SIZE as i32 {
            0.0
        } else {
            f32::from(MARK[y as usize * MARK_SIZE + x as usize]) / 255.0
        }
    };
    let (x0, y0) = ((u - 0.5).floor(), (v - 0.5).floor());
    let (fx, fy) = (u - 0.5 - x0, v - 0.5 - y0);
    let (x0, y0) = (x0 as i32, y0 as i32);
    let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx;
    let bottom = at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx;
    top * (1.0 - fy) + bottom * fy
}

fn in_rounded_square(x: f32, y: f32, size: f32, radius: f32) -> bool {
    let cx = x.clamp(radius, size - radius);
    let cy = y.clamp(radius, size - radius);
    (x - cx).powi(2) + (y - cy).powi(2) <= radius * radius
}

/// RGBA pixels of the icon, `size` x `size`, straight alpha. Geometry of
/// `favicon.svg`: a 64-unit tile with corner radius 14, the 66-unit mark
/// scaled by 0.697 at (9, 9).
pub fn rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 2; // supersampling of the tile edge
    let s = size as f32;
    let unit = s / 64.0;
    let radius = 14.0 * unit;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut inside = 0u32;
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    inside += u32::from(in_rounded_square(px, py, s, radius));
                }
            }
            let alpha = inside as f32 / (SS * SS) as f32;
            // Pixel centre in tile units, then in mask pixels.
            let tx = (x as f32 + 0.5) / unit;
            let ty = (y as f32 + 0.5) / unit;
            let scale = MARK_SIZE as f32 / (66.0 * 0.697);
            let a = coverage((tx - 9.0) * scale, (ty - 9.0) * scale);
            for c in TILE {
                out.push((c * (1.0 - a) + 255.0 * a).round() as u8);
            }
            out.push((alpha * 255.0).round() as u8);
        }
    }
    out
}

/// The window icon egui/winit takes (Dock, task bar, Alt-Tab).
pub fn window_icon(size: u32) -> egui::IconData {
    egui::IconData {
        rgba: rgba(size),
        width: size,
        height: size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mask_is_the_mark() {
        assert_eq!(MARK.len(), MARK_SIZE * MARK_SIZE);
        // The mark's outer frame is solid, its corners outside are empty.
        assert_eq!(MARK[0], 0, "outside the rounded frame");
        assert!(
            MARK[MARK_SIZE / 2 * MARK_SIZE + 4] > 200,
            "the left edge of the frame"
        );
        let covered = MARK.iter().filter(|a| **a > 127).count();
        // A line drawing: well under half of the square is ink.
        assert!((6000..32000).contains(&covered), "{covered}");
        assert_eq!(mark_image().pixels.len(), MARK_SIZE * MARK_SIZE);
    }

    #[test]
    fn the_icon_is_the_white_mark_on_the_dark_tile() {
        let size = 256;
        let px = rgba(size);
        assert_eq!(px.len(), (size * size * 4) as usize);
        let at = |x: u32, y: u32| {
            let i = ((y * size + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        assert_eq!(at(0, 0)[3], 0, "the corner outside the rounding");
        // Between the tile's edge and the mark (tile unit 4): dark ground.
        assert_eq!(at(16, 128), [10, 16, 20, 255]);
        // The mark's frame at its left edge (tile unit ~9.7): white.
        let frame = at(39, 128);
        assert!(frame[0] > 200 && frame[3] == 255, "{frame:?}");
    }
}
