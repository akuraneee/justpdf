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
    Caffeine,
    Pin,
    /// A document with a folded corner.
    File,
    /// Books on a shelf.
    Library,
    /// Trash can.
    Remove,
    /// Gear.
    Settings,
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
        Icon::Caffeine => {
            // Coffee cup with a handle and two wisps of steam.
            let cup = [(4.0, 10.0), (16.0, 10.0), (16.0, 16.0), (13.5, 19.5), (6.5, 19.5), (4.0, 16.0), (4.0, 10.0)];
            p.polyline(&cup.map(|(x, y)| pt(x, y)), t, color);
            p.polyline(&[pt(16.0, 11.5), pt(19.5, 11.5), pt(19.5, 15.0), pt(16.0, 15.0)], t, color);
            for x in [8.0, 12.0] {
                p.polyline(&[pt(x, 7.5), pt(x - 1.0, 6.0), pt(x, 4.5), pt(x - 1.0, 3.0)], 1.6 * k, color);
            }
        }
        Icon::Pin => {
            // Push pin: head, tapered body, needle.
            p.line(pt(8.5, 4.0).0, pt(8.5, 4.0).1, pt(15.5, 4.0).0, pt(15.5, 4.0).1, t, color);
            let body = [(10.0, 4.0), (10.0, 9.5), (6.5, 13.5), (17.5, 13.5), (14.0, 9.5), (14.0, 4.0)];
            p.polyline(&body.map(|(x, y)| pt(x, y)), t, color);
            let (a, b) = pt(12.0, 13.5);
            let (c, d) = pt(12.0, 20.5);
            p.line(a, b, c, d, t, color);
        }
        Icon::File => {
            let pts = [(14.0, 3.0), (6.0, 3.0), (6.0, 21.0), (18.0, 21.0), (18.0, 7.0), (14.0, 3.0), (14.0, 7.0), (18.0, 7.0)];
            p.polyline(&pts.map(|(x, y)| pt(x, y)), t, color);
        }
        Icon::Library => {
            // Two upright books and a leaning one.
            for x in [5.0, 9.5] {
                let r = [(x, 4.0), (x + 3.0, 4.0), (x + 3.0, 20.0), (x, 20.0), (x, 4.0)];
                p.polyline(&r.map(|(x, y)| pt(x, y)), t, color);
            }
            let lean = [(14.5, 6.5), (17.4, 5.7), (21.0, 19.2), (18.1, 20.0), (14.5, 6.5)];
            p.polyline(&lean.map(|(x, y)| pt(x, y)), t, color);
        }
        Icon::Remove => {
            let (a, b) = pt(4.0, 6.5);
            let (c, d) = pt(20.0, 6.5);
            p.line(a, b, c, d, t, color);
            p.polyline(&[pt(9.5, 6.5), pt(9.5, 3.5), pt(14.5, 3.5), pt(14.5, 6.5)], t, color);
            p.polyline(&[pt(6.5, 6.5), pt(7.5, 20.5), pt(16.5, 20.5), pt(17.5, 6.5)], t, color);
            for x in [10.5, 13.5] {
                let (a, b) = pt(x, 10.0);
                let (c, d) = pt(x, 17.0);
                p.line(a, b, c, d, 1.6 * k, color);
            }
        }
        Icon::Settings => {
            let (x, y) = pt(12.0, 12.0);
            p.ring(x, y, 3.0 * k, t, color);
            p.ring(x, y, 7.0 * k, t, color);
            for i in 0..8 {
                let a = i as f32 * std::f32::consts::FRAC_PI_4;
                let (c, s) = (a.cos(), a.sin());
                p.line(x + c * 7.5 * k, y + s * 7.5 * k, x + c * 10.0 * k, y + s * 10.0 * k, 2.6 * k, color);
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
