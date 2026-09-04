//! The two drawings a node card is made of: a minute of history as smoothed
//! area lines, and the radial ring in its header. Both are painted directly
//! rather than through a plotting crate — the card needs exactly these two
//! shapes, in these colours, with no axes, ticks or interaction, and a
//! plotting dependency would bring all of that plus a version to keep in
//! step with the GUI toolkit.

use eframe::egui::{self, Color32, Mesh, Pos2, Rect, Shape, Stroke, Vec2};

use super::theme;

/// One line on the chart.
pub struct Line<'a> {
    pub key: &'a str,
    pub color: Color32,
    /// 0–100, oldest first.
    pub points: Vec<f32>,
}

/// Draw the lines into `rect`. `solo` names the one line to show at full
/// strength while the others fade, the legend's click-to-isolate.
pub fn area_lines(ui: &mut egui::Ui, rect: Rect, lines: &[Line], solo: Option<&str>) {
    let painter = ui.painter_at(rect);
    painter.rect(
        rect,
        theme::RADIUS_SM,
        theme::CARD_RAISED,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Inside,
    );
    // Quarter lines, so 50% is readable without an axis.
    for q in [0.25, 0.5, 0.75] {
        let y = rect.bottom() - rect.height() * q;
        painter.line_segment(
            [
                Pos2::new(rect.left() + 1.0, y),
                Pos2::new(rect.right() - 1.0, y),
            ],
            Stroke::new(1.0, theme::with_alpha(theme::BORDER, 0x90)),
        );
    }

    let inner = rect.shrink2(Vec2::new(2.0, 4.0));
    for line in lines {
        if line.points.len() < 2 {
            continue;
        }
        let faded = solo.is_some_and(|s| s != line.key);
        let alpha_line = if faded { 0x30 } else { 0xff };
        let alpha_fill = if faded { 0x08 } else { 0x38 };
        let pts = smooth(&to_screen(&line.points, inner));

        // Fill under the curve as a strip of quads: each segment is convex
        // on its own, which a general polygon under a wiggly line is not.
        let mut mesh = Mesh::default();
        let fill = theme::with_alpha(line.color, alpha_fill);
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let base = inner.bottom();
            let i = mesh.vertices.len() as u32;
            mesh.colored_vertex(a, fill);
            mesh.colored_vertex(b, fill);
            mesh.colored_vertex(Pos2::new(b.x, base), fill);
            mesh.colored_vertex(Pos2::new(a.x, base), fill);
            mesh.add_triangle(i, i + 1, i + 2);
            mesh.add_triangle(i, i + 2, i + 3);
        }
        painter.add(Shape::mesh(mesh));
        painter.add(Shape::line(
            pts,
            Stroke::new(1.6, theme::with_alpha(line.color, alpha_line)),
        ));
    }
}

fn to_screen(points: &[f32], rect: Rect) -> Vec<Pos2> {
    let n = points.len().max(2) - 1;
    points
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = rect.left() + rect.width() * (i as f32 / n as f32);
            let y = rect.bottom() - rect.height() * (v.clamp(0.0, 100.0) / 100.0);
            Pos2::new(x, y)
        })
        .collect()
}

/// Catmull-Rom through the samples, four steps per segment. One sample a
/// second drawn as straight segments looks like a seismograph; the desktop
/// app smooths for the same reason.
fn smooth(pts: &[Pos2]) -> Vec<Pos2> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    const STEPS: usize = 4;
    let mut out = Vec::with_capacity(pts.len() * STEPS);
    let get = |i: isize| -> Pos2 {
        let i = i.clamp(0, pts.len() as isize - 1) as usize;
        pts[i]
    };
    for i in 0..pts.len() - 1 {
        let (p0, p1, p2, p3) = (
            get(i as isize - 1),
            get(i as isize),
            get(i as isize + 1),
            get(i as isize + 2),
        );
        for s in 0..STEPS {
            let t = s as f32 / STEPS as f32;
            let (t2, t3) = (t * t, t * t * t);
            let x = 0.5
                * ((2.0 * p1.x)
                    + (-p0.x + p2.x) * t
                    + (2.0 * p0.x - 5.0 * p1.x + 4.0 * p2.x - p3.x) * t2
                    + (-p0.x + 3.0 * p1.x - 3.0 * p2.x + p3.x) * t3);
            let y = 0.5
                * ((2.0 * p1.y)
                    + (-p0.y + p2.y) * t
                    + (2.0 * p0.y - 5.0 * p1.y + 4.0 * p2.y - p3.y) * t2
                    + (-p0.y + 3.0 * p1.y - 3.0 * p2.y + p3.y) * t3);
            out.push(Pos2::new(x, y));
        }
    }
    out.push(*pts.last().unwrap());
    out
}

/// Concentric rings, outermost first: each `(fraction 0–1, colour)`.
/// A ring with no reading draws its track only, which is the honest
/// picture of "nobody can tell you", not an empty ring that says idle.
pub fn rings(painter: &egui::Painter, center: Pos2, radius: f32, rings: &[(Option<f32>, Color32)]) {
    painter.circle_filled(
        center,
        radius + 3.0,
        theme::with_alpha(Color32::BLACK, 0x55),
    );
    let width = (radius / (rings.len() as f32 + 1.0)).clamp(3.0, 7.0);
    for (i, (value, color)) in rings.iter().enumerate() {
        let r = radius - i as f32 * (width + 1.5) - width / 2.0;
        if r <= 0.0 {
            break;
        }
        painter.circle_stroke(
            center,
            r,
            Stroke::new(width, theme::with_alpha(*color, 0x30)),
        );
        if let Some(v) = value {
            arc(
                painter,
                center,
                r,
                v.clamp(0.0, 1.0),
                Stroke::new(width, *color),
            );
        }
    }
}

/// An arc from twelve o'clock, clockwise, as a polyline: egui has no arc
/// primitive, and at these sizes forty segments are indistinguishable from
/// one.
fn arc(painter: &egui::Painter, center: Pos2, r: f32, fraction: f32, stroke: Stroke) {
    if fraction <= 0.0 {
        return;
    }
    let segments = (40.0 * fraction).ceil().max(2.0) as usize;
    let start = -std::f32::consts::FRAC_PI_2;
    let sweep = std::f32::consts::TAU * fraction;
    let pts: Vec<Pos2> = (0..=segments)
        .map(|i| {
            let a = start + sweep * (i as f32 / segments as f32);
            Pos2::new(center.x + r * a.cos(), center.y + r * a.sin())
        })
        .collect();
    painter.add(Shape::line(pts, stroke));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_map_to_the_rect_edges() {
        let rect = Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(100.0, 50.0));
        let pts = to_screen(&[0.0, 100.0], rect);
        assert_eq!(pts[0], Pos2::new(0.0, 50.0), "0% sits on the bottom edge");
        assert_eq!(pts[1], Pos2::new(100.0, 0.0), "100% sits on the top edge");
    }

    #[test]
    fn smoothing_keeps_the_endpoints_and_adds_points() {
        let pts = vec![
            Pos2::new(0.0, 10.0),
            Pos2::new(10.0, 0.0),
            Pos2::new(20.0, 10.0),
        ];
        let s = smooth(&pts);
        assert_eq!(s.first(), pts.first());
        assert_eq!(s.last(), pts.last());
        assert!(s.len() > pts.len());
    }
}
