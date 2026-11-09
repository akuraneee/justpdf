//! JNI entry points and the render thread that drives [`App`].
//!
//! Java forwards input, surface and lifecycle callbacks; everything is turned
//! into commands for a single Rust thread that owns the EGL context, the app
//! state and (through the app) the PDF worker.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jni::objects::{GlobalRef, JClass, JFloatArray, JIntArray, JObject, JString, JValue};
use jni::sys::{jboolean, jfloat, jint, jlong, jstring, JNI_FALSE, JNI_TRUE};
use jni::{JNIEnv, JavaVM};

use crate::android_codes;
use crate::app::{App, Command, Event};
use crate::egl::{Egl, EGL_CONTEXT_LOST};
use crate::gfx::Gpu;
use crate::gl::GlGpu;

#[link(name = "android")]
extern "C" {
    fn ANativeWindow_fromSurface(env: *mut c_void, surface: *mut c_void) -> *mut c_void;
    fn ANativeWindow_release(window: *mut c_void);
}

struct Window(*mut c_void);
// SAFETY: ANativeWindow is reference counted and usable from any thread.
unsafe impl Send for Window {}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe { ANativeWindow_release(self.0) };
    }
}

enum Cmd {
    App(Event),
    Surface(Window),
    SurfaceSize(u32, u32),
    SurfaceGone(SyncSender<()>),
    Quit,
}

#[derive(Default)]
struct Shared {
    back: AtomicBool,
    uri: Mutex<Option<String>>,
}

struct Handle {
    tx: Sender<Cmd>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

fn handle<'a>(h: jlong) -> Option<&'a Handle> {
    (h != 0).then(|| unsafe { &*(h as *const Handle) })
}

fn send(h: jlong, cmd: Cmd) {
    if let Some(h) = handle(h) {
        let _ = h.tx.send(cmd);
    }
}

fn now() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

fn jstr(env: &mut JNIEnv, s: &JString) -> String {
    if s.is_null() {
        return String::new();
    }
    env.get_string(s).map(Into::into).unwrap_or_default()
}

fn set_frame_rate(window: *mut c_void, fps: f32) {
    type SetFrameRate = unsafe extern "C" fn(*mut c_void, f32, i8) -> i32;
    static FUNC: OnceLock<Option<SetFrameRate>> = OnceLock::new();
    let f = FUNC.get_or_init(|| unsafe {
        let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() {
            return None;
        }
        let p = libc::dlsym(lib, c"ANativeWindow_setFrameRate".as_ptr());
        (!p.is_null()).then(|| std::mem::transmute::<*mut c_void, SetFrameRate>(p))
    });
    if let Some(f) = f {
        unsafe { f(window, fps, 0) };
    }
}

struct Jni {
    vm: JavaVM,
    activity: GlobalRef,
}

impl Jni {
    /// Runs an app command as a call on the activity (the Rust -> Java contract).
    fn run(&self, cmd: &Command) {
        let Ok(mut env) = self.vm.get_env() else { return };
        let activity = self.activity.as_obj();
        let _ = env.with_local_frame(8, |env| -> jni::errors::Result<()> {
            match cmd {
                Command::OpenPicker => env.call_method(activity, "openPicker", "()V", &[])?,
                Command::ShowKeyboard(kb, text) => {
                    let s = env.new_string(text)?;
                    let mode = JValue::Int(android_codes::ime_mode(*kb));
                    env.call_method(activity, "showKeyboard", "(ILjava/lang/String;)V", &[mode, JValue::Object(&s)])?
                }
                Command::HideKeyboard => env.call_method(activity, "hideKeyboard", "()V", &[])?,
                Command::SystemBars(visible) => {
                    env.call_method(activity, "setSystemBars", "(Z)V", &[JValue::Bool(*visible as jboolean)])?
                }
                Command::Exit => env.call_method(activity, "exitApp", "()V", &[])?,
            };
            Ok(())
        });
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_describe();
            let _ = env.exception_clear();
        }
    }
}

fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| crate::log_e!("panic: {info}")));
    });
}

// ----- render thread ---------------------------------------------------------------------

struct Gfx {
    egl: Egl,
    gpu: GlGpu,
}

fn create_gfx() -> Option<Gfx> {
    let egl = Egl::new(1, 1).map_err(|e| crate::log_e!("EGL: {e}")).ok()?;
    let gpu = GlGpu::new().map_err(|e| crate::log_e!("GL: {e}")).ok()?;
    Some(Gfx { egl, gpu })
}

#[allow(clippy::too_many_arguments)]
fn run(vm: JavaVM, activity: GlobalRef, rx: Receiver<Cmd>, tx: Sender<Cmd>, shared: Arc<Shared>, files: String, density: f32) {
    if let Err(e) = vm.attach_current_thread_permanently() {
        crate::log_e!("attach failed: {e}");
        return;
    }
    let platform = Jni { vm, activity };
    let sink_tx = Mutex::new(tx);
    let sink = Box::new(move |m| {
        let _ = sink_tx.lock().unwrap_or_else(|e| e.into_inner()).send(Cmd::App(Event::Worker(m)));
    });
    let mut app = App::new(density, Some(files.into()), sink);
    let mut gfx = create_gfx();
    let mut window: Option<Window> = None;
    let mut high_rate = false;
    let mut last_uri: Option<String> = None;

    'main: loop {
        let can_draw = window.is_some() && gfx.is_some();
        let first = if can_draw && app.needs_redraw() {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => break,
            }
        } else {
            match app.next_deadline() {
                Some(d) => match rx.recv_timeout(Duration::from_secs_f64((d - now()).clamp(0.0, 60.0))) {
                    Ok(c) => Some(c),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(c) => Some(c),
                    Err(_) => break,
                },
            }
        };
        let mut pending: Vec<Cmd> = first.into_iter().collect();
        pending.extend(rx.try_iter());
        for cmd in pending {
            match cmd {
                Cmd::App(ev) => app.handle(ev, now()),
                Cmd::Surface(w) => {
                    if let Some(g) = gfx.as_mut() {
                        g.egl.detach_window();
                    }
                    window = None;
                    if let Some(g) = gfx.as_mut() {
                        // SAFETY: `w` owns a reference to the window until it is dropped,
                        // and it is detached before that happens.
                        match unsafe { g.egl.attach_window(w.0) } {
                            Ok(()) => {
                                if let Some((sw, sh)) = g.egl.window_size() {
                                    app.resize(sw, sh);
                                }
                                app.invalidate();
                                window = Some(w);
                            }
                            Err(e) => crate::log_e!("{e}"),
                        }
                    }
                }
                Cmd::SurfaceSize(w, h) => app.resize(w, h),
                Cmd::SurfaceGone(ack) => {
                    if let Some(g) = gfx.as_mut() {
                        app.trim(&mut g.gpu);
                        g.egl.detach_window();
                    }
                    window = None;
                    let _ = ack.send(());
                }
                Cmd::Quit => break 'main,
            }
        }

        app.update(now());
        for cmd in app.drain_commands() {
            platform.run(&cmd);
        }
        shared.back.store(app.back_consumable(), Ordering::Relaxed);
        let uri = app.current_uri().map(str::to_owned);
        if uri != last_uri {
            *shared.uri.lock().unwrap_or_else(|e| e.into_inner()) = uri.clone();
            last_uri = uri;
        }

        if let (Some(w), Some(g)) = (window.as_ref(), gfx.as_mut()) {
            if app.needs_redraw() {
                app.render(&mut g.gpu);
                if let Err(err) = g.egl.swap() {
                    crate::log_w!("eglSwapBuffers failed: {err:#x}");
                    if err == EGL_CONTEXT_LOST {
                        let raw = w.0;
                        drop(gfx.take());
                        app.gpu_lost();
                        gfx = create_gfx();
                        if let Some(g) = gfx.as_mut() {
                            // SAFETY: the window is still referenced by `window`.
                            if unsafe { g.egl.attach_window(raw) }.is_err() {
                                window = None;
                            }
                        }
                    }
                }
            }
            if let Some(w) = window.as_ref() {
                let animating = app.animating();
                if animating != high_rate {
                    high_rate = animating;
                    set_frame_rate(w.0, if animating { 120.0 } else { 0.0 });
                }
            }
        }
    }

    app.shutdown(gfx.as_mut().map(|g| &mut g.gpu as &mut dyn Gpu));
    if let Some(g) = gfx.as_mut() {
        g.egl.detach_window();
    }
    drop(window);
    drop(gfx);
    drop(app);
}

// ----- JNI exports -----------------------------------------------------------------------

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeCreate<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    activity: JObject<'l>,
    files_dir: JString<'l>,
    density: jfloat,
) -> jlong {
    install_panic_hook();
    let vm = match env.get_java_vm() {
        Ok(vm) => vm,
        Err(e) => {
            crate::log_e!("no JavaVM: {e}");
            return 0;
        }
    };
    let activity = match env.new_global_ref(&activity) {
        Ok(a) => a,
        Err(e) => {
            crate::log_e!("global ref: {e}");
            return 0;
        }
    };
    let files = jstr(&mut env, &files_dir);
    let (tx, rx) = mpsc::channel();
    let shared = Arc::new(Shared::default());
    let (t2, s2) = (tx.clone(), shared.clone());
    let thread = std::thread::Builder::new().name("render".into()).spawn(move || run(vm, activity, rx, t2, s2, files, density)).ok();
    Box::into_raw(Box::new(Handle { tx, shared, thread })) as jlong
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeDestroy(_env: JNIEnv, _class: JClass, h: jlong) {
    if h == 0 {
        return;
    }
    let mut handle = unsafe { Box::from_raw(h as *mut Handle) };
    let _ = handle.tx.send(Cmd::Quit);
    if let Some(t) = handle.thread.take() {
        let _ = t.join();
    }
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeSurface(env: JNIEnv, _class: JClass, h: jlong, surface: JObject) {
    let Some(handle) = handle(h) else { return };
    if surface.is_null() {
        // Android requires the surface to be released before surfaceDestroyed returns.
        let (ack_tx, ack_rx) = mpsc::sync_channel(1);
        if handle.tx.send(Cmd::SurfaceGone(ack_tx)).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(3));
        }
        return;
    }
    let window = unsafe { ANativeWindow_fromSurface(env.get_raw().cast(), surface.as_raw().cast()) };
    if !window.is_null() {
        let _ = handle.tx.send(Cmd::Surface(Window(window)));
    }
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeSurfaceSize(_env: JNIEnv, _class: JClass, h: jlong, w: jint, hh: jint) {
    send(h, Cmd::SurfaceSize(w.max(1) as u32, hh.max(1) as u32));
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeInsets(
    _env: JNIEnv,
    _class: JClass,
    h: jlong,
    l: jint,
    t: jint,
    r: jint,
    b: jint,
) {
    send(h, Cmd::App(Event::Insets([l as f32, t as f32, r as f32, b as f32])));
}

#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeTouch<'l>(
    env: JNIEnv<'l>,
    _class: JClass<'l>,
    h: jlong,
    action: jint,
    index: jint,
    count: jint,
    ids: JIntArray<'l>,
    xy: JFloatArray<'l>,
    time_ms: jlong,
) {
    let Some(phase) = android_codes::touch_phase(action) else { return };
    let n = count.clamp(0, 16) as usize;
    let mut id = vec![0i32; n];
    let mut pos = vec![0f32; n * 2];
    if env.get_int_array_region(&ids, 0, &mut id).is_err() || env.get_float_array_region(&xy, 0, &mut pos).is_err() {
        return;
    }
    let pointers = (0..n).map(|i| (id[i], pos[2 * i], pos[2 * i + 1])).collect();
    send(h, Cmd::App(Event::Touch { phase, index: index.max(0) as usize, pointers, time: time_ms as f64 / 1000.0 }));
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeWheel(
    _env: JNIEnv,
    _class: JClass,
    h: jlong,
    x: jfloat,
    y: jfloat,
    delta: jfloat,
) {
    send(h, Cmd::App(Event::Wheel { x, y, delta }));
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeKey(_env: JNIEnv, _class: JClass, h: jlong, code: jint) {
    if let Some(key) = android_codes::key(code) {
        send(h, Cmd::App(Event::Key(key)));
    }
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeBack(_env: JNIEnv, _class: JClass, h: jlong) -> jboolean {
    let Some(handle) = handle(h) else { return JNI_FALSE };
    if handle.shared.back.load(Ordering::Relaxed) {
        let _ = handle.tx.send(Cmd::App(Event::Back));
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeOpen<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    h: jlong,
    fd: jint,
    uri: JString<'l>,
    name: JString<'l>,
) {
    let uri = jstr(&mut env, &uri);
    let name = jstr(&mut env, &name);
    match handle(h) {
        Some(handle) => {
            let _ = handle.tx.send(Cmd::App(Event::Open { fd, uri, name }));
        }
        None => unsafe {
            libc::close(fd);
        },
    }
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeOpenFailed<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    h: jlong,
    msg: JString<'l>,
) {
    let msg = jstr(&mut env, &msg);
    send(h, Cmd::App(Event::OpenFailed(msg)));
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativePickerCancelled(_env: JNIEnv, _class: JClass, h: jlong) {
    send(h, Cmd::App(Event::PickerCancelled));
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeText<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    h: jlong,
    text: JString<'l>,
    cursor: jint,
) {
    let text = jstr(&mut env, &text);
    send(h, Cmd::App(Event::Text { text, cursor: cursor.max(0) as usize }));
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeImeAction(_env: JNIEnv, _class: JClass, h: jlong) {
    send(h, Cmd::App(Event::ImeAction));
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativePause(_env: JNIEnv, _class: JClass, h: jlong) {
    send(h, Cmd::App(Event::Pause));
}

#[no_mangle]
pub extern "system" fn Java_dev_justpdf_MainActivity_nativeCurrentUri<'l>(env: JNIEnv<'l>, _class: JClass<'l>, h: jlong) -> jstring {
    let Some(handle) = handle(h) else { return std::ptr::null_mut() };
    let uri = handle.shared.uri.lock().unwrap_or_else(|e| e.into_inner()).clone();
    match uri {
        Some(u) => env.new_string(u).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut()),
        None => std::ptr::null_mut(),
    }
}
