//! OpenGL ES 3.0 implementation of [`Gpu`].

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::{c_char, c_void, CString};

use crate::gfx::{Gpu, TexId, UiVertex};

type GLenum = u32;
type GLuint = u32;
type GLint = i32;
type GLsizei = i32;
type GLboolean = u8;

const GL_COLOR_BUFFER_BIT: u32 = 0x4000;
const GL_TRIANGLES: GLenum = 0x0004;
const GL_TRIANGLE_STRIP: GLenum = 0x0005;
const GL_BLEND: GLenum = 0x0BE2;
const GL_SCISSOR_TEST: GLenum = 0x0C11;
const GL_DITHER: GLenum = 0x0BD0;
const GL_DEPTH_TEST: GLenum = 0x0B71;
const GL_CULL_FACE: GLenum = 0x0B44;
const GL_SRC_ALPHA: GLenum = 0x0302;
const GL_ONE_MINUS_SRC_ALPHA: GLenum = 0x0303;
const GL_TEXTURE_2D: GLenum = 0x0DE1;
const GL_TEXTURE0: GLenum = 0x84C0;
const GL_TEXTURE_MIN_FILTER: GLenum = 0x2801;
const GL_TEXTURE_MAG_FILTER: GLenum = 0x2800;
const GL_TEXTURE_WRAP_S: GLenum = 0x2802;
const GL_TEXTURE_WRAP_T: GLenum = 0x2803;
const GL_LINEAR: GLint = 0x2601;
const GL_CLAMP_TO_EDGE: GLint = 0x812F;
const GL_RGBA: GLenum = 0x1908;
const GL_RGBA8: GLenum = 0x8058;
const GL_R8: GLenum = 0x8229;
const GL_RED: GLenum = 0x1903;
const GL_UNSIGNED_BYTE: GLenum = 0x1401;
const GL_FLOAT: GLenum = 0x1406;
const GL_UNPACK_ALIGNMENT: GLenum = 0x0CF5;
const GL_PACK_ALIGNMENT: GLenum = 0x0D05;
const GL_ARRAY_BUFFER: GLenum = 0x8892;
const GL_STATIC_DRAW: GLenum = 0x88E4;
const GL_STREAM_DRAW: GLenum = 0x88E0;
const GL_VERTEX_SHADER: GLenum = 0x8B31;
const GL_FRAGMENT_SHADER: GLenum = 0x8B30;
const GL_COMPILE_STATUS: GLenum = 0x8B81;
const GL_LINK_STATUS: GLenum = 0x8B82;
const GL_MAX_TEXTURE_SIZE: GLenum = 0x0D33;

#[cfg_attr(target_os = "android", link(name = "GLESv3"))]
#[cfg_attr(not(target_os = "android"), link(name = "GLESv2"))]
extern "C" {
    fn glViewport(x: GLint, y: GLint, w: GLsizei, h: GLsizei);
    fn glScissor(x: GLint, y: GLint, w: GLsizei, h: GLsizei);
    fn glClearColor(r: f32, g: f32, b: f32, a: f32);
    fn glClear(mask: u32);
    fn glEnable(cap: GLenum);
    fn glDisable(cap: GLenum);
    fn glBlendFunc(s: GLenum, d: GLenum);
    fn glGenTextures(n: GLsizei, out: *mut GLuint);
    fn glDeleteTextures(n: GLsizei, t: *const GLuint);
    fn glBindTexture(target: GLenum, t: GLuint);
    fn glTexParameteri(target: GLenum, pname: GLenum, param: GLint);
    fn glTexStorage2D(target: GLenum, levels: GLsizei, fmt: GLenum, w: GLsizei, h: GLsizei);
    fn glTexSubImage2D(
        target: GLenum,
        level: GLint,
        x: GLint,
        y: GLint,
        w: GLsizei,
        h: GLsizei,
        fmt: GLenum,
        ty: GLenum,
        data: *const c_void,
    );
    fn glPixelStorei(pname: GLenum, param: GLint);
    fn glActiveTexture(t: GLenum);
    fn glCreateShader(ty: GLenum) -> GLuint;
    fn glShaderSource(s: GLuint, n: GLsizei, src: *const *const c_char, len: *const GLint);
    fn glCompileShader(s: GLuint);
    fn glGetShaderiv(s: GLuint, pname: GLenum, out: *mut GLint);
    fn glGetShaderInfoLog(s: GLuint, max: GLsizei, len: *mut GLsizei, log: *mut c_char);
    fn glDeleteShader(s: GLuint);
    fn glCreateProgram() -> GLuint;
    fn glAttachShader(p: GLuint, s: GLuint);
    fn glLinkProgram(p: GLuint);
    fn glGetProgramiv(p: GLuint, pname: GLenum, out: *mut GLint);
    fn glGetProgramInfoLog(p: GLuint, max: GLsizei, len: *mut GLsizei, log: *mut c_char);
    fn glUseProgram(p: GLuint);
    fn glGetUniformLocation(p: GLuint, name: *const c_char) -> GLint;
    fn glUniform1i(loc: GLint, v: GLint);
    fn glUniform2f(loc: GLint, a: f32, b: f32);
    fn glUniform4f(loc: GLint, a: f32, b: f32, c: f32, d: f32);
    fn glGenBuffers(n: GLsizei, out: *mut GLuint);
    fn glBindBuffer(target: GLenum, b: GLuint);
    fn glBufferData(target: GLenum, size: isize, data: *const c_void, usage: GLenum);
    fn glGenVertexArrays(n: GLsizei, out: *mut GLuint);
    fn glBindVertexArray(a: GLuint);
    fn glVertexAttribPointer(i: GLuint, size: GLint, ty: GLenum, norm: GLboolean, stride: GLsizei, ptr: *const c_void);
    fn glEnableVertexAttribArray(i: GLuint);
    fn glDrawArrays(mode: GLenum, first: GLint, count: GLsizei);
    fn glGetIntegerv(pname: GLenum, out: *mut GLint);
    fn glReadPixels(x: GLint, y: GLint, w: GLsizei, h: GLsizei, fmt: GLenum, ty: GLenum, data: *mut c_void);
    fn glFinish();
    fn glGetError() -> GLenum;
}

const TILE_VS: &str = r#"#version 300 es
layout(location = 0) in vec2 a_corner;
uniform vec2 u_screen;
uniform vec4 u_rect;
uniform vec4 u_uv;
out vec2 v_uv;
void main() {
    vec2 p = u_rect.xy + a_corner * u_rect.zw;
    v_uv = mix(u_uv.xy, u_uv.zw, a_corner);
    gl_Position = vec4(p.x / u_screen.x * 2.0 - 1.0, 1.0 - p.y / u_screen.y * 2.0, 0.0, 1.0);
}
"#;

const TILE_FS: &str = r#"#version 300 es
precision highp float;
uniform sampler2D u_tex;
uniform vec4 u_clamp;
in vec2 v_uv;
out vec4 o;
void main() {
    o = vec4(texture(u_tex, clamp(v_uv, u_clamp.xy, u_clamp.zw)).rgb, 1.0);
}
"#;

const UI_VS: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
layout(location = 2) in vec4 a_color;
layout(location = 3) in vec4 a_p;
uniform vec2 u_screen;
out vec2 v_uv;
out vec4 v_color;
out vec4 v_p;
void main() {
    v_uv = a_uv;
    v_color = a_color;
    v_p = a_p;
    gl_Position = vec4(a_pos.x / u_screen.x * 2.0 - 1.0, 1.0 - a_pos.y / u_screen.y * 2.0, 0.0, 1.0);
}
"#;

const UI_FS: &str = r#"#version 300 es
precision highp float;
uniform sampler2D u_atlas;
in vec2 v_uv;
in vec4 v_color;
in vec4 v_p;
out vec4 o;
void main() {
    float a;
    if (v_p.x < 0.5) {
        vec2 q = abs(v_uv) - v_p.yz + v_p.w;
        float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - v_p.w;
        a = clamp(0.5 - d, 0.0, 1.0);
    } else if (v_p.x < 1.5) {
        a = texture(u_atlas, v_uv).r;
    } else if (v_p.x < 2.5) {
        float d = abs(length(v_uv) - v_p.y) - v_p.z;
        a = clamp(0.5 - d, 0.0, 1.0);
    } else {
        vec2 q = vec2(max(abs(v_uv.x) - v_p.y, 0.0), v_uv.y);
        float d = length(q) - v_p.z;
        a = clamp(0.5 - d, 0.0, 1.0);
    }
    o = vec4(v_color.rgb, v_color.a * a);
}
"#;

unsafe fn compile(ty: GLenum, src: &str) -> Result<GLuint, String> {
    let s = glCreateShader(ty);
    let c = CString::new(src).unwrap();
    let ptr = c.as_ptr();
    glShaderSource(s, 1, &ptr, std::ptr::null());
    glCompileShader(s);
    let mut ok = 0;
    glGetShaderiv(s, GL_COMPILE_STATUS, &mut ok);
    if ok == 0 {
        let mut buf = vec![0u8; 4096];
        let mut len = 0;
        glGetShaderInfoLog(s, buf.len() as GLsizei, &mut len, buf.as_mut_ptr().cast());
        buf.truncate(len.max(0) as usize);
        glDeleteShader(s);
        return Err(format!("shader compile: {}", String::from_utf8_lossy(&buf)));
    }
    Ok(s)
}

unsafe fn program(vs: &str, fs: &str) -> Result<GLuint, String> {
    let v = compile(GL_VERTEX_SHADER, vs)?;
    let f = compile(GL_FRAGMENT_SHADER, fs)?;
    let p = glCreateProgram();
    glAttachShader(p, v);
    glAttachShader(p, f);
    glLinkProgram(p);
    glDeleteShader(v);
    glDeleteShader(f);
    let mut ok = 0;
    glGetProgramiv(p, GL_LINK_STATUS, &mut ok);
    if ok == 0 {
        let mut buf = vec![0u8; 4096];
        let mut len = 0;
        glGetProgramInfoLog(p, buf.len() as GLsizei, &mut len, buf.as_mut_ptr().cast());
        buf.truncate(len.max(0) as usize);
        return Err(format!("program link: {}", String::from_utf8_lossy(&buf)));
    }
    Ok(p)
}

unsafe fn uniform(p: GLuint, name: &str) -> GLint {
    let c = CString::new(name).unwrap();
    glGetUniformLocation(p, c.as_ptr())
}

pub struct GlGpu {
    tile_prog: GLuint,
    tile_screen: GLint,
    tile_rect: GLint,
    tile_uv: GLint,
    tile_clamp: GLint,
    ui_prog: GLuint,
    ui_screen: GLint,
    quad_vao: GLuint,
    ui_vao: GLuint,
    ui_vbo: GLuint,
    width: u32,
    height: u32,
    current: GLuint,
    blend: bool,
    pub max_texture: u32,
}

impl GlGpu {
    /// Creates programs and buffers. A GL ES 3 context must be current.
    pub fn new() -> Result<GlGpu, String> {
        unsafe {
            let tile_prog = program(TILE_VS, TILE_FS)?;
            let ui_prog = program(UI_VS, UI_FS)?;
            glUseProgram(tile_prog);
            glUniform1i(uniform(tile_prog, "u_tex"), 0);
            glUseProgram(ui_prog);
            glUniform1i(uniform(ui_prog, "u_atlas"), 0);

            let mut vao = [0; 2];
            glGenVertexArrays(2, vao.as_mut_ptr());
            let mut vbo = [0; 2];
            glGenBuffers(2, vbo.as_mut_ptr());

            glBindVertexArray(vao[0]);
            glBindBuffer(GL_ARRAY_BUFFER, vbo[0]);
            let corners: [f32; 8] = [0.0, 0.0, 1.0, 0.0, 0.0, 1.0, 1.0, 1.0];
            glBufferData(GL_ARRAY_BUFFER, std::mem::size_of_val(&corners) as isize, corners.as_ptr().cast(), GL_STATIC_DRAW);
            glEnableVertexAttribArray(0);
            glVertexAttribPointer(0, 2, GL_FLOAT, 0, 8, std::ptr::null());

            glBindVertexArray(vao[1]);
            glBindBuffer(GL_ARRAY_BUFFER, vbo[1]);
            let stride = std::mem::size_of::<UiVertex>() as GLsizei;
            glEnableVertexAttribArray(0);
            glVertexAttribPointer(0, 2, GL_FLOAT, 0, stride, std::ptr::null());
            glEnableVertexAttribArray(1);
            glVertexAttribPointer(1, 2, GL_FLOAT, 0, stride, 8 as *const c_void);
            glEnableVertexAttribArray(2);
            glVertexAttribPointer(2, 4, GL_UNSIGNED_BYTE, 1, stride, 16 as *const c_void);
            glEnableVertexAttribArray(3);
            glVertexAttribPointer(3, 4, GL_FLOAT, 0, stride, 20 as *const c_void);
            glBindVertexArray(0);

            let mut max_texture = 0;
            glGetIntegerv(GL_MAX_TEXTURE_SIZE, &mut max_texture);

            glDisable(GL_DEPTH_TEST);
            glDisable(GL_CULL_FACE);
            glDisable(GL_DITHER);
            glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
            glActiveTexture(GL_TEXTURE0);

            Ok(GlGpu {
                tile_screen: uniform(tile_prog, "u_screen"),
                tile_rect: uniform(tile_prog, "u_rect"),
                tile_uv: uniform(tile_prog, "u_uv"),
                tile_clamp: uniform(tile_prog, "u_clamp"),
                ui_screen: uniform(ui_prog, "u_screen"),
                tile_prog,
                ui_prog,
                quad_vao: vao[0],
                ui_vao: vao[1],
                ui_vbo: vbo[1],
                width: 1,
                height: 1,
                current: 0,
                blend: false,
                max_texture: max_texture.max(2048) as u32,
            })
        }
    }

    fn set_blend(&mut self, on: bool) {
        if self.blend != on {
            unsafe {
                if on {
                    glEnable(GL_BLEND)
                } else {
                    glDisable(GL_BLEND)
                }
            }
            self.blend = on;
        }
    }

    /// Returns (and clears) the pending GL error, 0 if none.
    pub fn error(&self) -> u32 {
        unsafe { glGetError() }
    }

    /// Reads the framebuffer as top-down RGBA (for tests and screenshots).
    pub fn read_pixels(&self) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut px = vec![0u8; w * h * 4];
        unsafe {
            glFinish();
            glPixelStorei(GL_PACK_ALIGNMENT, 1);
            glReadPixels(0, 0, w as GLsizei, h as GLsizei, GL_RGBA, GL_UNSIGNED_BYTE, px.as_mut_ptr().cast());
        }
        let mut flipped = vec![0u8; px.len()];
        for y in 0..h {
            flipped[y * w * 4..(y + 1) * w * 4].copy_from_slice(&px[(h - 1 - y) * w * 4..(h - y) * w * 4]);
        }
        flipped
    }
}

impl Gpu for GlGpu {
    fn begin(&mut self, width: u32, height: u32, c: [f32; 4]) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.current = 0;
        unsafe {
            glViewport(0, 0, self.width as GLsizei, self.height as GLsizei);
            glDisable(GL_SCISSOR_TEST);
            glClearColor(c[0], c[1], c[2], c[3]);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(self.tile_prog);
            glUniform2f(self.tile_screen, self.width as f32, self.height as f32);
            glUseProgram(self.ui_prog);
            glUniform2f(self.ui_screen, self.width as f32, self.height as f32);
            self.current = self.ui_prog;
        }
    }

    fn create_texture(&mut self, width: u32, height: u32, alpha_only: bool) -> TexId {
        let mut t = 0;
        unsafe {
            glGenTextures(1, &mut t);
            glBindTexture(GL_TEXTURE_2D, t);
            glTexStorage2D(GL_TEXTURE_2D, 1, if alpha_only { GL_R8 } else { GL_RGBA8 }, width as GLsizei, height as GLsizei);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        }
        t
    }

    fn upload(&mut self, tex: TexId, x: u32, y: u32, w: u32, h: u32, data: &[u8], alpha_only: bool) {
        let bpp = if alpha_only { 1 } else { 4 };
        assert!(data.len() >= (w * h * bpp) as usize, "upload buffer too small");
        unsafe {
            glBindTexture(GL_TEXTURE_2D, tex);
            glPixelStorei(GL_UNPACK_ALIGNMENT, if alpha_only { 1 } else { 4 });
            let fmt = if alpha_only { GL_RED } else { GL_RGBA };
            glTexSubImage2D(
                GL_TEXTURE_2D,
                0,
                x as GLint,
                y as GLint,
                w as GLsizei,
                h as GLsizei,
                fmt,
                GL_UNSIGNED_BYTE,
                data.as_ptr().cast(),
            );
        }
    }

    fn delete_texture(&mut self, tex: TexId) {
        unsafe { glDeleteTextures(1, &tex) };
    }

    fn draw_texture(&mut self, tex: TexId, r: [f32; 4], uv: [f32; 4], c: [f32; 4]) {
        self.set_blend(false);
        unsafe {
            if self.current != self.tile_prog {
                glUseProgram(self.tile_prog);
                glBindVertexArray(self.quad_vao);
                self.current = self.tile_prog;
            }
            glUniform4f(self.tile_rect, r[0], r[1], r[2], r[3]);
            glUniform4f(self.tile_uv, uv[0], uv[1], uv[2], uv[3]);
            glUniform4f(self.tile_clamp, c[0], c[1], c[2], c[3]);
            glBindTexture(GL_TEXTURE_2D, tex);
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        }
    }

    fn clip(&mut self, r: Option<[f32; 4]>) {
        unsafe {
            match r {
                Some([x, y, w, h]) => {
                    // GL counts rows from the bottom.
                    let (x0, x1) = (x.round() as GLint, (x + w).round() as GLint);
                    let (y0, y1) = (y.round() as GLint, (y + h).round() as GLint);
                    glEnable(GL_SCISSOR_TEST);
                    glScissor(x0, self.height as GLint - y1, (x1 - x0).max(0), (y1 - y0).max(0));
                }
                None => glDisable(GL_SCISSOR_TEST),
            }
        }
    }

    fn draw_ui(&mut self, v: &[UiVertex], atlas: TexId) {
        if v.is_empty() {
            return;
        }
        self.set_blend(true);
        unsafe {
            glUseProgram(self.ui_prog);
            self.current = self.ui_prog;
            glBindVertexArray(self.ui_vao);
            glBindBuffer(GL_ARRAY_BUFFER, self.ui_vbo);
            glBufferData(GL_ARRAY_BUFFER, std::mem::size_of_val(v) as isize, v.as_ptr().cast(), GL_STREAM_DRAW);
            glBindTexture(GL_TEXTURE_2D, atlas);
            glDrawArrays(GL_TRIANGLES, 0, v.len() as GLsizei);
        }
    }
}
