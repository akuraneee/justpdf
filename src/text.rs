//! UI text: system fonts (memory-mapped), a glyph atlas and simple layout.

use std::collections::HashMap;
use std::os::fd::AsRawFd;

use ab_glyph::{point, Font, FontRef, GlyphId, PxScale, ScaleFont};

use crate::gfx::{Gpu, Painter, Rgba, TexId};

const ATLAS: u32 = 1024;

const PRIMARY: &[&str] = &[
    "/system/fonts/Roboto-Regular.ttf",
    "/system/fonts/RobotoStatic-Regular.ttf",
    "/system/fonts/NotoSans-Regular.ttf",
    "/system/fonts/DroidSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/Roboto-Regular.ttf",
];

const FALLBACK: &[&str] = &[
    "/system/fonts/NotoSansCJK-Regular.ttc",
    "/system/fonts/NotoSansSC-Regular.otf",
    "/system/fonts/DroidSansFallback.ttf",
    "/system/fonts/DroidSansFallbackFull.ttf",
    "/system/fonts/NotoSansSymbols-Regular-Subsetted.ttf",
    "/system/fonts/NotoSansSymbols-Regular-Subsetted2.ttf",
    "/system/fonts/NotoSansHebrew-Regular.ttf",
    "/system/fonts/NotoSansThai-Regular.ttf",
    "/system/fonts/NotoSansDevanagari-Regular.otf",
    "/system/fonts/NotoNaskhArabic-Regular.ttf",
    "/system/fonts/NotoSansArmenian-Regular.otf",
    "/system/fonts/NotoSansGeorgian-Regular.otf",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
];

fn map_file(path: &str) -> Option<&'static [u8]> {
    let file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len() as usize;
    if len < 64 {
        return None;
    }
    unsafe {
        let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0);
        if p == libc::MAP_FAILED {
            return None;
        }
        Some(std::slice::from_raw_parts(p as *const u8, len))
    }
}

/// Maps and parses a font, keeping it only if `usable` accepts it (the mapping
/// of a rejected file is released).
fn load_face_if(path: &str, usable: impl Fn(&FontRef) -> bool) -> Option<FontRef<'static>> {
    let data = map_file(path)?;
    let face = FontRef::try_from_slice_and_index(data, 0).ok().filter(|f| usable(f));
    if face.is_none() {
        unsafe { libc::munmap(data.as_ptr() as *mut libc::c_void, data.len()) };
    }
    face
}

fn load_face(path: &str) -> Option<FontRef<'static>> {
    load_face_if(path, |_| true)
}

fn scan_for_face(dir: &str) -> Option<FontRef<'static>> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    entries
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "ttf"))
        .find_map(|p| load_face_if(p.to_str()?, |f| f.glyph_id('a').0 != 0 && f.glyph_id('0').0 != 0))
}

#[derive(Clone, Copy, Debug)]
struct GlyphInfo {
    uv: [f32; 4],
    ox: f32,
    oy: f32,
    w: f32,
    h: f32,
}

pub struct Text {
    faces: Vec<FontRef<'static>>,
    fallbacks_loaded: bool,
    chars: HashMap<char, (u8, GlyphId)>,
    glyphs: HashMap<(u8, u16, u16), Option<GlyphInfo>>,
    pen_x: u32,
    pen_y: u32,
    row_h: u32,
    uploads: Vec<(u32, u32, u32, u32, Vec<u8>)>,
    tex: Option<TexId>,
    /// Atlas overflowed this frame; cleared at the start of the next one.
    overflow: bool,
}

impl Default for Text {
    fn default() -> Self {
        Self::new()
    }
}

impl Text {
    pub fn new() -> Text {
        let mut faces: Vec<FontRef<'static>> = PRIMARY.iter().find_map(|p| load_face(p)).into_iter().collect();
        if faces.is_empty() {
            faces.extend(scan_for_face("/system/fonts"));
        }
        if faces.is_empty() {
            crate::log_w!("no usable system font found; UI text disabled");
        }
        Text {
            faces,
            fallbacks_loaded: false,
            chars: HashMap::new(),
            glyphs: HashMap::new(),
            pen_x: 0,
            pen_y: 0,
            row_h: 0,
            uploads: Vec::new(),
            tex: None,
            overflow: false,
        }
    }

    /// Call at the start of each frame. Returns true if the atlas was reset
    /// (the previous frame may have missing glyphs and should be redrawn).
    pub fn begin_frame(&mut self) -> bool {
        if !self.overflow {
            return false;
        }
        self.overflow = false;
        self.glyphs.clear();
        self.pen_x = 0;
        self.pen_y = 0;
        self.row_h = 0;
        true
    }

    fn scale(face: &FontRef<'static>, px: f32) -> PxScale {
        PxScale::from(px * face.height_unscaled() / face.units_per_em().unwrap_or(1000.0))
    }

    fn lookup(&mut self, c: char) -> Option<(u8, GlyphId)> {
        if let Some(&r) = self.chars.get(&c) {
            return Some(r);
        }
        if self.faces.is_empty() {
            return None;
        }
        let mut found = self.faces.iter().position(|f| f.glyph_id(c).0 != 0);
        if found.is_none() && !self.fallbacks_loaded && !c.is_whitespace() && !c.is_control() {
            self.fallbacks_loaded = true;
            let start = self.faces.len();
            self.faces.extend(FALLBACK.iter().filter_map(|p| load_face(p)));
            found = self.faces[start..].iter().position(|f| f.glyph_id(c).0 != 0).map(|i| i + start);
        }
        let fi = found.unwrap_or(0);
        let r = (fi as u8, self.faces[fi].glyph_id(c));
        self.chars.insert(c, r);
        Some(r)
    }

    /// Lays out `s` at size `px`: glyphs with x offsets, and the total advance.
    fn shape(&mut self, s: &str, px: f32) -> (Vec<(u8, GlyphId, f32)>, f32) {
        let mut out = Vec::with_capacity(s.len());
        let mut x = 0.0;
        let mut prev: Option<(u8, GlyphId)> = None;
        for c in s.chars() {
            let c = if c.is_control() { ' ' } else { c };
            let Some((fi, gid)) = self.lookup(c) else { continue };
            let face = &self.faces[fi as usize];
            let sf = face.as_scaled(Self::scale(face, px));
            if let Some((pf, pg)) = prev {
                if pf == fi {
                    x += sf.kern(pg, gid);
                }
            }
            out.push((fi, gid, x));
            x += sf.h_advance(gid);
            prev = Some((fi, gid));
        }
        (out, x)
    }

    pub fn measure(&mut self, s: &str, px: f32) -> f32 {
        self.shape(s, px).1
    }

    /// Distance from baseline to the top of capital letters, roughly.
    pub fn ascent(&self, px: f32) -> f32 {
        match self.faces.first() {
            Some(f) => f.as_scaled(Self::scale(f, px)).ascent(),
            None => px * 0.9,
        }
    }

    /// Height to centre text vertically: baseline = centre_y + cap_height / 2.
    pub fn cap_height(&self, px: f32) -> f32 {
        px * 0.71
    }

    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        if w + 1 > ATLAS || h + 1 > ATLAS {
            return None;
        }
        if self.pen_x + w + 1 > ATLAS {
            self.pen_x = 0;
            self.pen_y += self.row_h + 1;
            self.row_h = 0;
        }
        if self.pen_y + h + 1 > ATLAS {
            self.overflow = true;
            return None;
        }
        let p = (self.pen_x, self.pen_y);
        self.pen_x += w + 1;
        self.row_h = self.row_h.max(h);
        Some(p)
    }

    fn glyph(&mut self, fi: u8, gid: GlyphId, px: f32) -> Option<GlyphInfo> {
        let key = (fi, gid.0, (px * 4.0).round().min(65535.0) as u16);
        if let Some(g) = self.glyphs.get(&key) {
            return *g;
        }
        let face = self.faces[fi as usize].clone();
        let glyph = gid.with_scale_and_position(Self::scale(&face, px), point(0.0, 0.0));
        let Some(outline) = face.outline_glyph(glyph) else {
            self.glyphs.insert(key, None);
            return None;
        };
        let b = outline.px_bounds();
        let (w, h) = (b.width() as u32, b.height() as u32);
        if w == 0 || h == 0 {
            self.glyphs.insert(key, None);
            return None;
        }
        let (ax, ay) = self.alloc(w, h)?;
        let mut buf = vec![0u8; (w * h) as usize];
        outline.draw(|x, y, c| {
            if let Some(p) = buf.get_mut((y * w + x) as usize) {
                *p = (c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        });
        self.uploads.push((ax, ay, w, h, buf));
        let a = ATLAS as f32;
        let info = GlyphInfo {
            uv: [ax as f32 / a, ay as f32 / a, (ax + w) as f32 / a, (ay + h) as f32 / a],
            ox: b.min.x,
            oy: b.min.y,
            w: w as f32,
            h: h as f32,
        };
        self.glyphs.insert(key, Some(info));
        Some(info)
    }

    /// Draws `s` with its baseline at `y`, truncating with an ellipsis to
    /// `max_w`. Returns the drawn width.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, p: &mut Painter, s: &str, x: f32, y: f32, px: f32, color: Rgba, max_w: f32) -> f32 {
        let (mut glyphs, mut width) = self.shape(s, px);
        if width > max_w {
            let (ell, ell_w) = self.shape("…", px);
            // End x of glyph i is the start of glyph i + 1 (kerning included).
            let end_of = |i: usize| glyphs.get(i + 1).map_or(width, |g| g.2);
            let keep = (0..glyphs.len()).take_while(|&i| end_of(i) + ell_w <= max_w).count();
            let end = if keep == 0 { 0.0 } else { end_of(keep - 1) };
            glyphs.truncate(keep);
            glyphs.extend(ell.into_iter().map(|(fi, gid, gx)| (fi, gid, end + gx)));
            width = end + ell_w;
        }
        let (bx, by) = (x.round(), y.round());
        for (fi, gid, gx) in glyphs {
            if let Some(g) = self.glyph(fi, gid, px) {
                p.glyph((bx + gx).round() + g.ox, by + g.oy, g.w, g.h, g.uv, color);
            }
        }
        width
    }

    /// Uploads newly rasterised glyphs; returns the atlas texture.
    pub fn flush(&mut self, gpu: &mut dyn Gpu) -> TexId {
        let tex = match self.tex {
            Some(t) => t,
            None => {
                let t = gpu.create_texture(ATLAS, ATLAS, true);
                gpu.upload(t, 0, 0, ATLAS, ATLAS, &vec![0u8; (ATLAS * ATLAS) as usize], true);
                self.tex = Some(t);
                t
            }
        };
        for (x, y, w, h, buf) in self.uploads.drain(..) {
            gpu.upload(tex, x, y, w, h, &buf, true);
        }
        tex
    }

    /// The GL context was lost: forget the atlas so it is rebuilt.
    pub fn forget_gpu(&mut self) {
        self.tex = None;
        self.glyphs.clear();
        self.uploads.clear();
        self.pen_x = 0;
        self.pen_y = 0;
        self.row_h = 0;
    }
}
