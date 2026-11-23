//! The small drawings a card needs: a meter bar, and a minute of history
//! as an area line. Painted directly (no plotting crate): no axes, ticks or
//! interaction are wanted, and a plotting dependency would bring all of
//! that plus a version to keep in step with egui.

use eframe::egui::{self, Color32, Mesh, Pos2, Rect, Shape, Stroke, Vec2};

use super::theme;

/// A horizontal meter, 0-100, the width of the row.
pub fn meter(ui: &mut egui::Ui, percent: f32, color: Color32) {
    let p = theme::palette(ui.ctx());
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, p.track);
    let frac = (percent / 100.0).clamp(0.0, 1.0);
    if frac > 0.0 {
        let mut full = rect;
        full.set_width(rect.width() * frac);
        painter.rect_filled(full, 4.0, color);
    }
}

/// A minute of `points` (0-100, oldest first) as a filled line.
pub fn sparkline(ui: &mut egui::Ui, points: &[f32], color: Color32, height: f32) {
    let p = theme::palette(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), height),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    painter.rect(
        rect,
        4.0,
        p.card,
        Stroke::new(1.0, p.line),
        egui::StrokeKind::Inside,
    );
    if points.len() < 2 {
        return;
    }
    let inner = rect.shrink2(Vec2::new(2.0, 3.0));
    let pts = to_screen(points, inner);
    let fill = theme::with_alpha(color, 0x38);
    let mut mesh = Mesh::default();
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
    painter.add(Shape::line(pts, Stroke::new(1.5, color)));
}

fn to_screen(points: &[f32], rect: Rect) -> Vec<Pos2> {
    let n = (points.len().max(2) - 1) as f32;
    points
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = rect.left() + rect.width() * (i as f32 / n);
            let y = rect.bottom() - rect.height() * (v.clamp(0.0, 100.0) / 100.0);
            Pos2::new(x, y)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_span_the_rect_and_clamp() {
        let r = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(100.0, 50.0));
        let p = to_screen(&[0.0, 50.0, 150.0], r);
        assert_eq!(p[0], Pos2::new(0.0, 50.0));
        assert_eq!(p[1], Pos2::new(50.0, 25.0));
        assert_eq!(p[2], Pos2::new(100.0, 0.0));
    }
}
