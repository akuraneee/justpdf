//! Continuous vertical ("webtoon") layout, the view transform and tile grids.
//!
//! Document space is measured in "base pixels": at zoom 1 each row of pages
//! spans the viewport width, and rows are stacked with no gap. A row holds one
//! page, or two side by side in spread mode (scaled to a common height so that
//! together they fill the width). The view maps document space to the screen:
//! `screen = (doc - view.xy) * zoom`.

pub const MIN_ZOOM: f64 = 0.5;
pub const MAX_ZOOM: f64 = 8.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Row {
    y: f64,
    h: f64,
    first: usize,
    end: usize,
}

#[derive(Clone, Debug)]
pub struct Layout {
    /// Content width (the viewport width at zoom 1).
    pub width: f64,
    /// Per page: left edge, width, top, height.
    pub xs: Vec<f64>,
    pub widths: Vec<f64>,
    pub tops: Vec<f64>,
    pub heights: Vec<f64>,
    rows: Vec<Row>,
    pub total: f64,
}

impl Layout {
    pub fn new(sizes: &[(f32, f32)], width: f64, spread: bool) -> Layout {
        let width = width.max(1.0);
        let n = sizes.len();
        let aspect = |i: usize| {
            let (w, h) = sizes[i];
            ((w as f64).max(1e-3) / (h as f64).max(1e-3)).clamp(1.0 / 400.0, 100.0)
        };
        let mut l = Layout {
            width,
            xs: Vec::with_capacity(n),
            widths: Vec::with_capacity(n),
            tops: Vec::with_capacity(n),
            heights: Vec::with_capacity(n),
            rows: Vec::with_capacity(if spread { n.div_ceil(2) } else { n }),
            total: 0.0,
        };
        let mut i = 0;
        let mut y = 0.0;
        while i < n {
            let k = if spread { (n - i).min(2) } else { 1 };
            let sum: f64 = (i..i + k).map(aspect).sum();
            // A lone page in spread mode takes half the width, like its neighbours.
            let h = if spread && k == 1 { width / 2.0 / sum } else { width / sum };
            let mut x = (width - h * sum) / 2.0;
            for j in i..i + k {
                let w = h * aspect(j);
                l.xs.push(x);
                l.widths.push(w);
                l.tops.push(y);
                l.heights.push(h);
                x += w;
            }
            l.rows.push(Row { y, h, first: i, end: i + k });
            y += h;
            i += k;
        }
        l.total = y;
        l
    }

    pub fn len(&self) -> usize {
        self.tops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tops.is_empty()
    }

    fn row_at(&self, y: f64) -> Option<&Row> {
        let last = self.rows.len().checked_sub(1)?;
        self.rows.get(self.rows.partition_point(|r| r.y + r.h <= y).min(last))
    }

    /// First page of the row containing document y (clamped to valid pages).
    pub fn page_at(&self, y: f64) -> usize {
        self.row_at(y).map_or(0, |r| r.first)
    }

    /// The pages sharing a row with `page`.
    pub fn row_pages(&self, page: usize) -> std::ops::Range<usize> {
        let i = self.rows.partition_point(|r| r.end <= page);
        self.rows.get(i).map_or(page..page + 1, |r| r.first..r.end)
    }

    /// Pages intersecting the document y range `[y0, y1)`.
    pub fn range(&self, y0: f64, y1: f64) -> std::ops::Range<usize> {
        if self.rows.is_empty() || y1 <= 0.0 || y0 >= self.total || y1 <= y0 {
            return 0..0;
        }
        let r0 = self.rows.partition_point(|r| r.y + r.h <= y0);
        let r1 = self.rows.partition_point(|r| r.y < y1).max(r0 + 1).min(self.rows.len());
        self.rows[r0].first..self.rows[r1 - 1].end
    }

    /// Document y of a position inside a page (`frac` from the top).
    pub fn y_of(&self, page: usize, frac: f64) -> f64 {
        match self.tops.get(page) {
            Some(&t) => t + self.heights[page] * frac.clamp(0.0, 1.0),
            None => self.total,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub zoom: f64,
    pub x: f64,
    pub y: f64,
}

impl Default for View {
    fn default() -> Self {
        View { zoom: 1.0, x: 0.0, y: 0.0 }
    }
}

impl View {
    pub fn to_doc(&self, sx: f64, sy: f64) -> (f64, f64) {
        (sx / self.zoom + self.x, sy / self.zoom + self.y)
    }

    pub fn to_screen(&self, dx: f64, dy: f64) -> (f64, f64) {
        ((dx - self.x) * self.zoom, (dy - self.y) * self.zoom)
    }

    /// Same view at zoom `z`, keeping the document point under screen (`fx`, `fy`) fixed.
    pub fn zoomed_at(&self, z: f64, fx: f64, fy: f64) -> View {
        let (dx, dy) = self.to_doc(fx, fy);
        View { zoom: z, x: dx - fx / z, y: dy - fy / z }
    }

    /// Clamps zoom and scroll so content stays on screen. Content narrower or
    /// shorter than the viewport is centred.
    pub fn clamped(mut self, l: &Layout, vw: f64, vh: f64) -> View {
        self.zoom = self.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let (w, h) = (vw / self.zoom, vh / self.zoom);
        self.x = if w >= l.width { (l.width - w) / 2.0 } else { self.x.clamp(0.0, l.width - w) };
        self.y = if h >= l.total { (l.total - h) / 2.0 } else { self.y.clamp(0.0, l.total - h) };
        self
    }
}

/// Tiles are rendered at discrete scales `2^(level/4)` relative to the
/// fit-width size, so zooming reuses tiles and a level is at most ~19% sharper
/// than needed.
pub fn level_for_zoom(zoom: f64) -> i8 {
    (4.0 * zoom.max(1e-3).log2() - 0.05).ceil().clamp(-12.0, 13.0) as i8
}

pub fn level_scale(level: i8) -> f64 {
    (level as f64 / 4.0).exp2()
}

pub const TILE_H: u32 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub page_w: u32,
    pub page_h: u32,
    pub tile_w: u32,
    pub tile_h: u32,
    pub cols: u32,
    pub rows: u32,
}

impl Grid {
    /// Pixel rectangle (x, y, w, h) of tile (`col`, `row`).
    pub fn tile_rect(&self, col: u32, row: u32) -> (u32, u32, u32, u32) {
        let x = col * self.tile_w;
        let y = row * self.tile_h;
        (x, y, self.tile_w.min(self.page_w - x), self.tile_h.min(self.page_h - y))
    }

    /// Tiles intersecting the page-fraction rectangle (`u0`, `v0`)-(`u1`, `v1`).
    pub fn cover(&self, u0: f64, v0: f64, u1: f64, v1: f64) -> Option<(std::ops::Range<u32>, std::ops::Range<u32>)> {
        let (u0, v0, u1, v1) = (u0.max(0.0), v0.max(0.0), u1.min(1.0), v1.min(1.0));
        if u1 <= u0 || v1 <= v0 {
            return None;
        }
        let c0 = ((u0 * self.page_w as f64) / self.tile_w as f64).floor() as u32;
        let c1 = (((u1 * self.page_w as f64) / self.tile_w as f64).ceil() as u32).min(self.cols);
        let r0 = ((v0 * self.page_h as f64) / self.tile_h as f64).floor() as u32;
        let r1 = (((v1 * self.page_h as f64) / self.tile_h as f64).ceil() as u32).min(self.rows);
        (c0 < c1 && r0 < r1).then_some((c0..c1, r0..r1))
    }
}

pub fn grid(l: &Layout, page: usize, level: i8) -> Grid {
    let s = level_scale(level);
    let page_w = (l.widths[page] * s).round().max(1.0) as u32;
    let page_h = (l.heights[page] * s).round().clamp(1.0, 1_000_000.0) as u32;
    let max_w = if level <= 0 { 2048 } else { 1024 };
    let cols = page_w.div_ceil(max_w);
    let tile_w = page_w.div_ceil(cols);
    let tile_h = TILE_H.min(page_h);
    let rows = page_h.div_ceil(tile_h);
    Grid { page_w, page_h, tile_w, tile_h, cols, rows }
}

/// Size of the low-resolution placeholder rendered for a whole page.
pub fn thumb_dims(l: &Layout, page: usize) -> (u32, u32) {
    let aspect = l.heights[page] / l.widths[page];
    let mut w = (l.widths[page] / 4.0).clamp(48.0, 512.0);
    let mut h = w * aspect;
    let max_px = 360_000.0;
    if w * h > max_px {
        let k = (max_px / (w * h)).sqrt();
        w *= k;
        h *= k;
    }
    if h > 2048.0 {
        w *= 2048.0 / h;
        h = 2048.0;
    }
    (w.round().max(8.0) as u32, h.round().max(8.0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_stacks_pages() {
        let l = Layout::new(&[(100.0, 200.0), (200.0, 100.0), (100.0, 100.0)], 1000.0, false);
        assert_eq!(l.heights, vec![2000.0, 500.0, 1000.0]);
        assert_eq!(l.tops, vec![0.0, 2000.0, 2500.0]);
        assert_eq!(l.total, 3500.0);
        assert_eq!(l.page_at(0.0), 0);
        assert_eq!(l.page_at(1999.0), 0);
        assert_eq!(l.page_at(2000.0), 1);
        assert_eq!(l.page_at(1e9), 2);
        assert_eq!(l.range(1900.0, 2600.0), 0..3);
        assert_eq!(l.range(2000.0, 2400.0), 1..2);
    }

    #[test]
    fn spread_pairs_pages() {
        // Two portrait pages share a row; a trailing landscape page sits alone, centred at half width.
        let l = Layout::new(&[(100.0, 200.0), (100.0, 200.0), (200.0, 100.0)], 1000.0, true);
        assert_eq!(l.xs, vec![0.0, 500.0, 250.0]);
        assert_eq!(l.widths, vec![500.0, 500.0, 500.0]);
        assert_eq!(l.tops, vec![0.0, 0.0, 1000.0]);
        assert_eq!(l.heights, vec![1000.0, 1000.0, 250.0]);
        assert_eq!(l.total, 1250.0);
        assert_eq!(l.page_at(500.0), 0);
        assert_eq!(l.page_at(1100.0), 2);
        assert_eq!(l.range(0.0, 10.0), 0..2);
        assert_eq!(l.range(900.0, 1100.0), 0..3);
        assert_eq!(l.row_pages(1), 0..2);
        assert_eq!(l.row_pages(2), 2..3);
        // Pages of different shapes get a common height and fill the width together.
        let l = Layout::new(&[(100.0, 100.0), (200.0, 100.0)], 900.0, true);
        assert_eq!(l.heights, vec![300.0, 300.0]);
        assert_eq!((l.widths[0], l.widths[1], l.xs[1]), (300.0, 600.0, 300.0));
    }

    #[test]
    fn levels() {
        assert_eq!(level_for_zoom(1.0), 0);
        assert_eq!(level_for_zoom(1.1), 1);
        assert_eq!(level_for_zoom(2.0), 4);
        assert_eq!(level_for_zoom(0.5), -4);
        assert!(level_scale(level_for_zoom(3.3)) >= 3.3);
    }

    #[test]
    fn view_clamp_and_zoom() {
        let l = Layout::new(&[(100.0, 100.0); 10], 1000.0, false);
        let v = View { zoom: 1.0, x: 50.0, y: -100.0 }.clamped(&l, 1000.0, 2000.0);
        assert_eq!((v.x, v.y), (0.0, 0.0));
        let v = View { zoom: 1.0, x: 0.0, y: 1e9 }.clamped(&l, 1000.0, 2000.0);
        assert_eq!(v.y, 8000.0);
        let z = v.zoomed_at(2.0, 500.0, 1000.0);
        let (a, b) = v.to_doc(500.0, 1000.0);
        let (c, d) = z.to_doc(500.0, 1000.0);
        assert!((a - c).abs() < 1e-9 && (b - d).abs() < 1e-9);
        // Zoomed out: centred horizontally.
        let v = View { zoom: 0.5, x: 0.0, y: 0.0 }.clamped(&l, 1000.0, 2000.0);
        assert_eq!(v.x, -500.0);
    }

    #[test]
    fn grid_covers_page() {
        let l = Layout::new(&[(612.0, 792.0)], 1080.0, false);
        let g = grid(&l, 0, 0);
        assert_eq!((g.page_w, g.cols, g.tile_w), (1080, 1, 1080));
        assert_eq!(g.rows, g.page_h.div_ceil(TILE_H));
        let (x, y, w, h) = g.tile_rect(0, g.rows - 1);
        assert_eq!((x, w), (0, 1080));
        assert_eq!(y + h, g.page_h);
        let g = grid(&l, 0, 8); // 4x
        assert!(g.tile_w <= 1024 && g.cols * g.tile_w >= g.page_w);
        let (cols, rows) = g.cover(0.25, 0.5, 0.5, 0.75).unwrap();
        assert!(cols.start <= cols.end && rows.start < rows.end);
        assert!(g.cover(0.5, 0.5, 0.5, 0.6).is_none());
    }
}
