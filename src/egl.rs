//! EGL context management: one GLES 3 context, a 1x1 pbuffer to keep it
//! current while there is no window, and the window surface when there is.

use std::ffi::c_void;
use std::ptr::null_mut;

type EGLDisplay = *mut c_void;
type EGLConfig = *mut c_void;
type EGLContext = *mut c_void;
type EGLSurface = *mut c_void;
type EGLBoolean = u32;
type EGLint = i32;

const EGL_NONE: EGLint = 0x3038;
const EGL_RED_SIZE: EGLint = 0x3024;
const EGL_GREEN_SIZE: EGLint = 0x3023;
const EGL_BLUE_SIZE: EGLint = 0x3022;
const EGL_ALPHA_SIZE: EGLint = 0x3021;
const EGL_DEPTH_SIZE: EGLint = 0x3025;
const EGL_STENCIL_SIZE: EGLint = 0x3026;
const EGL_SURFACE_TYPE: EGLint = 0x3033;
const EGL_WINDOW_BIT: EGLint = 0x0004;
const EGL_PBUFFER_BIT: EGLint = 0x0001;
const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
const EGL_OPENGL_ES3_BIT: EGLint = 0x0040;
const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
const EGL_WIDTH: EGLint = 0x3057;
const EGL_HEIGHT: EGLint = 0x3056;
#[cfg(target_os = "android")]
const EGL_NATIVE_VISUAL_ID: EGLint = 0x302E;
pub const EGL_CONTEXT_LOST: EGLint = 0x300E;
const EGL_OPENGL_ES_API: u32 = 0x30A0;

#[link(name = "EGL")]
extern "C" {
    fn eglGetDisplay(native: *mut c_void) -> EGLDisplay;
    fn eglInitialize(d: EGLDisplay, major: *mut EGLint, minor: *mut EGLint) -> EGLBoolean;
    fn eglBindAPI(api: u32) -> EGLBoolean;
    fn eglChooseConfig(d: EGLDisplay, attribs: *const EGLint, configs: *mut EGLConfig, size: EGLint, num: *mut EGLint) -> EGLBoolean;
    fn eglGetConfigAttrib(d: EGLDisplay, c: EGLConfig, attr: EGLint, value: *mut EGLint) -> EGLBoolean;
    fn eglCreateContext(d: EGLDisplay, c: EGLConfig, share: EGLContext, attribs: *const EGLint) -> EGLContext;
    fn eglCreateWindowSurface(d: EGLDisplay, c: EGLConfig, win: *mut c_void, attribs: *const EGLint) -> EGLSurface;
    fn eglCreatePbufferSurface(d: EGLDisplay, c: EGLConfig, attribs: *const EGLint) -> EGLSurface;
    fn eglMakeCurrent(d: EGLDisplay, draw: EGLSurface, read: EGLSurface, ctx: EGLContext) -> EGLBoolean;
    fn eglSwapBuffers(d: EGLDisplay, s: EGLSurface) -> EGLBoolean;
    fn eglSwapInterval(d: EGLDisplay, interval: EGLint) -> EGLBoolean;
    fn eglDestroySurface(d: EGLDisplay, s: EGLSurface) -> EGLBoolean;
    fn eglDestroyContext(d: EGLDisplay, c: EGLContext) -> EGLBoolean;
    fn eglTerminate(d: EGLDisplay) -> EGLBoolean;
    fn eglGetError() -> EGLint;
    fn eglQuerySurface(d: EGLDisplay, s: EGLSurface, attr: EGLint, value: *mut EGLint) -> EGLBoolean;
}

#[cfg(target_os = "android")]
#[link(name = "android")]
extern "C" {
    fn ANativeWindow_setBuffersGeometry(window: *mut c_void, w: i32, h: i32, format: i32) -> i32;
}

pub struct Egl {
    display: EGLDisplay,
    config: EGLConfig,
    context: EGLContext,
    pbuffer: EGLSurface,
    window: EGLSurface,
}

impl Egl {
    /// Creates a GLES 3 context and makes it current on a small pbuffer.
    pub fn new(pbuffer_w: i32, pbuffer_h: i32) -> Result<Egl, String> {
        unsafe {
            let display = eglGetDisplay(null_mut());
            if display.is_null() {
                return Err("eglGetDisplay failed".into());
            }
            let (mut major, mut minor) = (0, 0);
            if eglInitialize(display, &mut major, &mut minor) == 0 {
                return Err(format!("eglInitialize failed: {:#x}", eglGetError()));
            }
            eglBindAPI(EGL_OPENGL_ES_API);
            let mut configs = [null_mut(); 64];
            let mut n = 0;
            for surface_bits in [EGL_WINDOW_BIT | EGL_PBUFFER_BIT, EGL_WINDOW_BIT, EGL_PBUFFER_BIT] {
                let attribs = [
                    EGL_RENDERABLE_TYPE,
                    EGL_OPENGL_ES3_BIT,
                    EGL_SURFACE_TYPE,
                    surface_bits,
                    EGL_RED_SIZE,
                    8,
                    EGL_GREEN_SIZE,
                    8,
                    EGL_BLUE_SIZE,
                    8,
                    EGL_DEPTH_SIZE,
                    0,
                    EGL_STENCIL_SIZE,
                    0,
                    EGL_NONE,
                ];
                if eglChooseConfig(display, attribs.as_ptr(), configs.as_mut_ptr(), configs.len() as EGLint, &mut n) != 0 && n > 0 {
                    break;
                }
            }
            if n <= 0 {
                return Err(format!("no suitable EGL config: {:#x}", eglGetError()));
            }
            let attr = |c: EGLConfig, a: EGLint| {
                let mut v = 0;
                eglGetConfigAttrib(display, c, a, &mut v);
                v
            };
            // Prefer exactly RGB888 without alpha, then RGBA8888 (EGL sorts deeper
            // formats such as 10-bit first, which we do not want).
            let candidates = &configs[..n as usize];
            let rgb8 = |c: EGLConfig| attr(c, EGL_RED_SIZE) == 8 && attr(c, EGL_GREEN_SIZE) == 8 && attr(c, EGL_BLUE_SIZE) == 8;
            let config = candidates
                .iter()
                .copied()
                .find(|&c| rgb8(c) && attr(c, EGL_ALPHA_SIZE) == 0)
                .or_else(|| candidates.iter().copied().find(|&c| rgb8(c) && attr(c, EGL_ALPHA_SIZE) == 8))
                .unwrap_or(candidates[0]);
            crate::log_i!(
                "EGL {major}.{minor} config: r{} g{} b{} a{}",
                attr(config, EGL_RED_SIZE),
                attr(config, EGL_GREEN_SIZE),
                attr(config, EGL_BLUE_SIZE),
                attr(config, EGL_ALPHA_SIZE)
            );
            let ctx_attribs = [EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE];
            let context = eglCreateContext(display, config, null_mut(), ctx_attribs.as_ptr());
            if context.is_null() {
                return Err(format!("eglCreateContext failed: {:#x}", eglGetError()));
            }
            let pb_attribs = [EGL_WIDTH, pbuffer_w.max(1), EGL_HEIGHT, pbuffer_h.max(1), EGL_NONE];
            let pbuffer = eglCreatePbufferSurface(display, config, pb_attribs.as_ptr());
            let egl = Egl { display, config, context, pbuffer, window: null_mut() };
            if eglMakeCurrent(display, pbuffer, pbuffer, context) == 0 {
                return Err(format!("eglMakeCurrent failed: {:#x}", eglGetError()));
            }
            Ok(egl)
        }
    }

    /// Attaches an `ANativeWindow*` and makes it current.
    ///
    /// # Safety
    /// `window` must be a valid `ANativeWindow*` that outlives the attachment.
    pub unsafe fn attach_window(&mut self, window: *mut c_void) -> Result<(), String> {
        self.detach_window();
        unsafe {
            #[cfg(target_os = "android")]
            {
                let mut format = 0;
                eglGetConfigAttrib(self.display, self.config, EGL_NATIVE_VISUAL_ID, &mut format);
                ANativeWindow_setBuffersGeometry(window, 0, 0, format);
            }
            let surface = eglCreateWindowSurface(self.display, self.config, window, [EGL_NONE].as_ptr());
            if surface.is_null() {
                return Err(format!("eglCreateWindowSurface failed: {:#x}", eglGetError()));
            }
            if eglMakeCurrent(self.display, surface, surface, self.context) == 0 {
                let err = eglGetError();
                eglDestroySurface(self.display, surface);
                return Err(format!("eglMakeCurrent(window) failed: {err:#x}"));
            }
            eglSwapInterval(self.display, 1);
            self.window = surface;
        }
        Ok(())
    }

    /// Releases the window surface, falling back to the pbuffer.
    pub fn detach_window(&mut self) {
        if self.window.is_null() {
            return;
        }
        unsafe {
            eglMakeCurrent(self.display, self.pbuffer, self.pbuffer, self.context);
            eglDestroySurface(self.display, self.window);
        }
        self.window = null_mut();
    }

    pub fn has_window(&self) -> bool {
        !self.window.is_null()
    }

    pub fn window_size(&self) -> Option<(u32, u32)> {
        if self.window.is_null() {
            return None;
        }
        let (mut w, mut h) = (0, 0);
        unsafe {
            eglQuerySurface(self.display, self.window, EGL_WIDTH, &mut w);
            eglQuerySurface(self.display, self.window, EGL_HEIGHT, &mut h);
        }
        Some((w.max(1) as u32, h.max(1) as u32))
    }

    /// Presents the window surface. Returns the EGL error on failure.
    pub fn swap(&self) -> Result<(), EGLint> {
        if self.window.is_null() {
            return Ok(());
        }
        unsafe {
            if eglSwapBuffers(self.display, self.window) == 0 {
                return Err(eglGetError());
            }
        }
        Ok(())
    }
}

impl Drop for Egl {
    fn drop(&mut self) {
        self.detach_window();
        unsafe {
            eglMakeCurrent(self.display, null_mut(), null_mut(), null_mut());
            if !self.pbuffer.is_null() {
                eglDestroySurface(self.display, self.pbuffer);
            }
            eglDestroyContext(self.display, self.context);
            eglTerminate(self.display);
        }
    }
}
