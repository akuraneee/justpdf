//! Backend-neutral drawing interface plus a batched 2D painter.
//!
//! The app draws page tiles as textured quads and everything else (UI,
//! highlights, text) as one vertex batch whose fragment shader evaluates
//! anti-aliased signed distance shapes: rounded rects, rings and capsules.

pub type TexId = u32;
pub type Rgba = [u8; 4];

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct UiVertex {
    pub pos: [f32; 2],
    /// Local shape coordinates (px from shape centre) or atlas UV for glyphs.
    pub uv: [f32; 2],
    pub color: Rgba,
    /// `[mode, a, b, c]`: 0 rounded rect (half w, half h, radius),
    /// 1 glyph, 2 ring (radius, half thickness), 3 capsule (half length, half thickness).
    pub p: [f32; 4],
}

pub trait Gpu {
    fn begin(&mut self, width: u32, height: u32, clear: [f32; 4]);
    fn create_texture(&mut self, width: u32, height: u32, alpha_only: bool) -> TexId;
    #[allow(clippy::too_many_arguments)]
    fn upload(&mut self, tex: TexId, x: u32, y: u32, w: u32, h: u32, data: &[u8], alpha_only: bool);
    fn delete_texture(&mut self, tex: TexId);
    /// Draws `tex` into screen rect `[x, y, w, h]` sampling UV rect `[u0, v0, u1, v1]`,
    /// never sampling outside `clamp` (also a UV rect) so filtering cannot bleed in
    /// texels from outside the valid area.
    fn draw_texture(&mut self, tex: TexId, rect: [f32; 4], uv: [f32; 4], clamp: [f32; 4]);
    fn draw_ui(&mut self, vertices: &[UiVertex], atlas: TexId);
}

pub fn alpha(c: Rgba, a: f32) -> Rgba {
    [c[0], c[1], c[2], (c[3] as f32 * a.clamp(0.0, 1.0)).round() as u8]
}

#[derive(Default)]
pub struct Painter {
    pub v: Vec<UiVertex>,
}

impl Painter {
    pub fn clear(&mut self) {
        self.v.clear();
    }

    fn quad(&mut self, corners: [[f32; 2]; 4], uvs: [[f32; 2]; 4], color: Rgba, p: [f32; 4]) {
        if color[3] == 0 {
            return;
        }
        for i in [0, 1, 2, 0, 2, 3] {
            self.v.push(UiVertex { pos: corners[i], uv: uvs[i], color, p });
        }
    }

    /// Filled rectangle with optional corner radius.
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgba, radius: f32) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let (hw, hh) = (w / 2.0, h / 2.0);
        let (ex, ey) = (hw + 1.0, hh + 1.0);
        let (cx, cy) = (x + hw, y + hh);
        let r = radius.min(hw).min(hh).max(0.0);
        self.quad(
            [[cx - ex, cy - ey], [cx + ex, cy - ey], [cx + ex, cy + ey], [cx - ex, cy + ey]],
            [[-ex, -ey], [ex, -ey], [ex, ey], [-ex, ey]],
            color,
            [0.0, hw, hh, r],
        );
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, color: Rgba) {
        self.rect(cx - r, cy - r, 2.0 * r, 2.0 * r, color, r);
    }

    pub fn ring(&mut self, cx: f32, cy: f32, r: f32, thickness: f32, color: Rgba) {
        let e = r + thickness / 2.0 + 1.0;
        self.quad(
            [[cx - e, cy - e], [cx + e, cy - e], [cx + e, cy + e], [cx - e, cy + e]],
            [[-e, -e], [e, -e], [e, e], [-e, e]],
            color,
            [2.0, r, thickness / 2.0, 0.0],
        );
    }

    /// Line segment with round caps.
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, thickness: f32, color: Rgba) {
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len = (dx * dx + dy * dy).sqrt();
        let (ux, uy) = if len > 1e-4 { (dx / len, dy / len) } else { (1.0, 0.0) };
        let (px, py) = (-uy, ux);
        let (mx, my) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let half = len / 2.0;
        let t = thickness / 2.0;
        let (ex, ey) = (half + t + 1.0, t + 1.0);
        let local = [[-ex, -ey], [ex, -ey], [ex, ey], [-ex, ey]];
        let corners = local.map(|[lx, ly]| [mx + ux * lx + px * ly, my + uy * lx + py * ly]);
        self.quad(corners, local, color, [3.0, half, t, 0.0]);
    }

    /// Connected line segments.
    pub fn polyline(&mut self, pts: &[(f32, f32)], thickness: f32, color: Rgba) {
        for w in pts.windows(2) {
            self.line(w[0].0, w[0].1, w[1].0, w[1].1, thickness, color);
        }
    }

    pub fn glyph(&mut self, x: f32, y: f32, w: f32, h: f32, uv: [f32; 4], color: Rgba) {
        self.quad(
            [[x, y], [x + w, y], [x + w, y + h], [x, y + h]],
            [[uv[0], uv[1]], [uv[2], uv[1]], [uv[2], uv[3]], [uv[0], uv[3]]],
            color,
            [1.0, 0.0, 0.0, 0.0],
        );
    }
}

/// GPU that does nothing; used by tests.
#[derive(Default)]
pub struct NullGpu {
    next: TexId,
    pub live: std::collections::HashSet<TexId>,
    pub ui_vertices: usize,
    pub textures_drawn: usize,
}

impl Gpu for NullGpu {
    fn begin(&mut self, _: u32, _: u32, _: [f32; 4]) {
        self.ui_vertices = 0;
        self.textures_drawn = 0;
    }
    fn create_texture(&mut self, _: u32, _: u32, _: bool) -> TexId {
        self.next += 1;
        self.live.insert(self.next);
        self.next
    }
    fn upload(&mut self, tex: TexId, _: u32, _: u32, _: u32, _: u32, _: &[u8], _: bool) {
        assert!(self.live.contains(&tex), "upload to dead texture");
    }
    fn delete_texture(&mut self, tex: TexId) {
        assert!(self.live.remove(&tex), "double delete");
    }
    fn draw_texture(&mut self, tex: TexId, _: [f32; 4], _: [f32; 4], _: [f32; 4]) {
        assert!(self.live.contains(&tex), "draw of dead texture");
        self.textures_drawn += 1;
    }
    fn draw_ui(&mut self, v: &[UiVertex], _: TexId) {
        self.ui_vertices += v.len();
    }
}
