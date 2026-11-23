//! The window icon, rasterised here rather than shipped as a file: the
//! primary blue diamond on the dark rounded tile of the KMPLIFY app icon
//! (the owner's design from the router branch, in the dashboard's tokens).

/// RGBA pixels, `size` x `size`, straight alpha.
pub fn rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 4; // supersampling per axis
    let s = size as f32;
    let radius = s * 0.22;
    let half = s * 0.30;
    let centre = s / 2.0;
    let bg = [10.0f32, 16.0, 20.0]; // --bg, dark
    let fg = [37.0f32, 99.0, 235.0]; // --accent
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (mut in_bg, mut in_fg) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let px = x as f32 + (sx as f32 + 0.5) / SS as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / SS as f32;
                    if in_rounded_square(px, py, s, radius) {
                        in_bg += 1;
                        if (px - centre).abs() + (py - centre).abs() <= half {
                            in_fg += 1;
                        }
                    }
                }
            }
            let alpha = in_bg as f32 / (SS * SS) as f32;
            let blue = if in_bg == 0 {
                0.0
            } else {
                in_fg as f32 / in_bg as f32
            };
            for c in 0..3 {
                out.push((bg[c] * (1.0 - blue) + fg[c] * blue).round() as u8);
            }
            out.push((alpha * 255.0).round() as u8);
        }
    }
    out
}

fn in_rounded_square(x: f32, y: f32, size: f32, radius: f32) -> bool {
    let cx = x.clamp(radius, size - radius);
    let cy = y.clamp(radius, size - radius);
    (x - cx).powi(2) + (y - cy).powi(2) <= radius * radius
}

pub fn window_icon(size: u32) -> eframe::egui::IconData {
    eframe::egui::IconData {
        rgba: rgba(size),
        width: size,
        height: size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_corner_blue_centre_dark_edge() {
        let size = 64;
        let px = rgba(size);
        let at = |x: u32, y: u32| {
            let i = ((y * size + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        assert_eq!(at(0, 0)[3], 0);
        assert_eq!(at(32, 32), [37, 99, 235, 255]);
        assert_eq!(at(6, 32), [10, 16, 20, 255]);
    }
}
