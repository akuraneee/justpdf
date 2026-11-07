//! Palette and vector icons for the overlay.

use crate::gfx::{alpha, Painter, Rgba};

pub const BG: [f32; 4] = [0.071, 0.071, 0.078, 1.0];
pub const PLACEHOLDER: Rgba = [232, 232, 234, 255];
pub const BAR: Rgba = [22, 22, 25, 255];
pub const PANEL: Rgba = [28, 28, 32, 255];
pub const FG: Rgba = [242, 242, 245, 255];
pub const DIM: Rgba = [150, 150, 158, 255];
pub const ACCENT: Rgba = [138, 180, 248, 255];
pub const PRESS: Rgba = [255, 255, 255, 40];
pub const FIELD: Rgba = [255, 255, 255, 22];
pub const ROW_SEL: Rgba = [138, 180, 248, 30];
pub const HIT: Rgba = [255, 210, 60, 100];
pub const HIT_CURRENT: Rgba = [255, 120, 0, 140];
pub const THUMB: Rgba = [255, 255, 255, 150];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Open,
    Search,
    Chapters,
    Back,
    Up,
    Down,
    Spread,
}

/// Draws `icon` centred at (`cx`, `cy`); `size` is the 24-unit design grid in px.
pub fn icon(p: &mut Painter, icon: Icon, cx: f32, cy: f32, size: f32, color: Rgba) {
    let k = size / 24.0;
    let pt = |x: f32, y: f32| (cx + (x - 12.0) * k, cy + (y - 12.0) * k);
    let t = 2.0 * k;
    match icon {
        Icon::Open => {
            let pts = [(3.0, 6.0), (9.5, 6.0), (11.5, 8.5), (21.0, 8.5), (21.0, 18.5), (3.0, 18.5), (3.0, 6.0)];
            p.polyline(&pts.map(|(x, y)| pt(x, y)), t, color);
        }
        Icon::Search => {
            let (x, y) = pt(10.5, 10.5);
            p.ring(x, y, 6.0 * k, t, color);
            let (a, b) = pt(15.0, 15.0);
            let (c, d) = pt(20.0, 20.0);
            p.line(a, b, c, d, t, color);
        }
        Icon::Chapters => {
            for y in [7.0, 12.0, 17.0] {
                let (x0, y0) = pt(4.5, y);
                p.circle(x0, y0, 1.3 * k, color);
                let (a, b) = pt(8.5, y);
                let (c, d) = pt(20.0, y);
                p.line(a, b, c, d, t, color);
            }
        }
        Icon::Spread => {
            for x0 in [3.0, 13.0] {
                let r = [(x0, 5.0), (x0 + 8.0, 5.0), (x0 + 8.0, 19.0), (x0, 19.0), (x0, 5.0)];
                p.polyline(&r.map(|(x, y)| pt(x, y)), t, color);
            }
        }
        Icon::Back => {
            let (a, b) = pt(19.5, 12.0);
            let (c, d) = pt(5.0, 12.0);
            p.line(a, b, c, d, t, color);
            p.polyline(&[pt(11.0, 6.0), pt(5.0, 12.0), pt(11.0, 18.0)], t, color);
        }
        Icon::Up => p.polyline(&[pt(6.0, 15.0), pt(12.0, 9.0), pt(18.0, 15.0)], t, color),
        Icon::Down => p.polyline(&[pt(6.0, 9.0), pt(12.0, 15.0), pt(18.0, 9.0)], t, color),
    }
}

pub fn fade(c: Rgba, a: f32) -> Rgba {
    alpha(c, a)
}
