//! The application icon, rasterised here rather than shipped as a file: a
//! blue diamond on the dark rounded square the top bar also paints. One
//! source for the window icon, the tray icon and the packaging assets
//! (`packaging/icons/`, rendered from the same geometry), so they cannot
//! drift apart.

/// RGBA pixels, `size` × `size`, premultiplication-free, straight alpha.
pub fn rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 4; // supersampling per axis
    let s = size as f32;
    let radius = s * 0.22;
    let half = s * 0.30;
    let centre = s / 2.0;
    let bg = [17.0f32, 24.0, 39.0]; // cinder 900
    let fg = [59.0f32, 130.0, 246.0]; // blue 500
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
            let samples = (SS * SS) as f32;
            let alpha = in_bg as f32 / samples;
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

/// The window icon egui/winit takes.
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
    fn the_icon_is_opaque_in_the_middle_transparent_at_the_corner_and_blue_at_the_centre() {
        let size = 64;
        let px = rgba(size);
        let at = |x: u32, y: u32| {
            let i = ((y * size + x) * 4) as usize;
            [px[i], px[i + 1], px[i + 2], px[i + 3]]
        };
        assert_eq!(
            at(0, 0)[3],
            0,
            "the corner outside the rounding is transparent"
        );
        assert_eq!(
            at(32, 32),
            [59, 130, 246, 255],
            "the diamond's centre is the primary blue"
        );
        assert_eq!(
            at(6, 32),
            [17, 24, 39, 255],
            "the edge midpoint is the dark ground"
        );
        assert_eq!(px.len(), (size * size * 4) as usize);
    }
}
