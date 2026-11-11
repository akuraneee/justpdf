//! Renders the real app offscreen (EGL pbuffer + GLES3 via Mesa) and writes
//! PNG screenshots. Used to check rendering and UI without a device.
//!
//! cargo run --release --features headless --example headless -- file.pdf out_dir \
//!     shot:open drag:-2000 wait:500 shot:scrolled tap:540,1200 ...
//!
//! With a directory instead of a PDF the app starts in its library with that
//! directory added as a library folder (fresh settings each run); the mock
//! platform answers scans, opens and cover requests from the file system.
//!
//! Window size: JUSTPDF_HEADLESS_SIZE=WxH (default 1080x2340, density 2.75).
//! Commands: shot:NAME, wait:MS, drag:DY, dragx:DX, fling:DY, tap:X,Y, dtap:X,Y,
//! swipe:X,Y,DX,DY, pinch:X,Y,FACTOR, text:STRING, ime, back, key:CODE, idle,
//! nothing (launch with nothing to open), cancel (file picker cancelled)

use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use justpdf::app::{self, App, Event, Platform};
use justpdf::egl::Egl;
use justpdf::gl::GlGpu;

const DENSITY: f32 = 2.75;

/// Window size: 1080×2340 (portrait phone) unless `JUSTPDF_HEADLESS_SIZE=WxH`.
fn size() -> (u32, u32) {
    std::env::var("JUSTPDF_HEADLESS_SIZE")
        .ok()
        .and_then(|s| s.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
        .unwrap_or((1080, 2340))
}

struct Mock {
    log: Vec<String>,
    tx: mpsc::Sender<Event>,
}

fn open_path(uri: &str) -> Option<i32> {
    let c = std::ffi::CString::new(uri.strip_prefix("file://")?).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    (fd >= 0).then_some(fd)
}

/// PDFs under `dir`: (folder path relative to `root`, file name, file:// URI).
fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<(String, String, String)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(root, &p, out);
        } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("pdf")) {
            let rel = dir.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            out.push((rel, e.file_name().to_string_lossy().into_owned(), format!("file://{}", p.display())));
        }
    }
}

impl Platform for Mock {
    fn open_picker(&mut self) {
        self.log.push("open_picker".into());
    }
    fn open_folder_picker(&mut self) {
        self.log.push("open_folder_picker".into());
    }
    fn scan_folder(&mut self, uri: &str) {
        self.log.push(format!("scan_folder({uri})"));
        let root = std::path::PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri));
        let mut books = Vec::new();
        walk(&root, &root, &mut books);
        let _ = self.tx.send(Event::FolderScanned { uri: uri.to_string(), books, ok: root.is_dir() });
    }
    fn release_folder(&mut self, uri: &str) {
        self.log.push(format!("release_folder({uri})"));
    }
    fn open_document(&mut self, uri: &str) {
        self.log.push(format!("open_document({uri})"));
        let ev = match open_path(uri) {
            Some(fd) => {
                let name = uri.rsplit('/').next().unwrap_or("Document").to_string();
                Event::Open { fd, uri: uri.to_string(), name }
            }
            None => Event::OpenFailed(uri.to_string()),
        };
        let _ = self.tx.send(ev);
    }
    fn open_cover(&mut self, key: u64, uri: &str) {
        let _ = self.tx.send(Event::CoverFd { key, fd: open_path(uri).unwrap_or(-1) });
    }
    fn show_keyboard(&mut self, mode: i32, text: &str) {
        self.log.push(format!("show_keyboard({mode}, {text:?})"));
    }
    fn hide_keyboard(&mut self) {
        self.log.push("hide_keyboard".into());
    }
    fn system_bars(&mut self, visible: bool) {
        self.log.push(format!("system_bars({visible})"));
    }
    fn keep_screen_on(&mut self, on: bool) {
        self.log.push(format!("keep_screen_on({on})"));
    }
    fn exit(&mut self) {
        self.log.push("exit".into());
    }
}

struct Harness {
    app: App,
    gpu: GlGpu,
    rx: mpsc::Receiver<Event>,
    mock: Mock,
    start: Instant,
    touch_t: f64,
}

impl Harness {
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn pump(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            let t = self.now();
            self.app.handle(ev, t, &mut self.mock);
        }
        let t = self.now();
        self.app.update(t, &mut self.mock);
        if self.app.needs_redraw() {
            self.app.render(&mut self.gpu);
        }
    }

    /// Runs the loop for `ms` milliseconds of wall time.
    fn run_for(&mut self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            self.pump();
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    /// Runs until nothing is animating or loading (or a timeout).
    fn settle(&mut self) {
        let end = Instant::now() + Duration::from_secs(20);
        let mut quiet = 0;
        while Instant::now() < end {
            self.pump();
            if self.app.needs_redraw() {
                quiet = 0;
            } else {
                quiet += 1;
                if quiet > 60 {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn touch(&mut self, action: i32, index: usize, pts: &[(f32, f32)]) {
        self.touch_t += 0.008;
        let pointers = pts.iter().enumerate().map(|(i, &(x, y))| (i as i32, x, y)).collect();
        let t = self.now();
        self.app.handle(Event::Touch { action, index, pointers, time: self.touch_t }, t, &mut self.mock);
        self.app.update(t, &mut self.mock);
        self.app.render(&mut self.gpu);
    }

    fn drag(&mut self, x0: f32, y0: f32, dx: f32, dy: f32, steps: usize, step_ms: u64) {
        self.touch(app::ACTION_DOWN, 0, &[(x0, y0)]);
        for i in 1..=steps {
            let f = i as f32 / steps as f32;
            std::thread::sleep(Duration::from_millis(step_ms));
            self.touch(app::ACTION_MOVE, 0, &[(x0 + dx * f, y0 + dy * f)]);
        }
        self.touch(app::ACTION_UP, 0, &[(x0 + dx, y0 + dy)]);
    }

    fn tap(&mut self, x: f32, y: f32) {
        self.touch(app::ACTION_DOWN, 0, &[(x, y)]);
        self.touch(app::ACTION_UP, 0, &[(x, y)]);
    }

    fn shot(&mut self, path: &str) {
        let before = self.gpu.error();
        self.app.render(&mut self.gpu);
        let err = self.gpu.error();
        if before != 0 || err != 0 {
            eprintln!("GL error before={before:#x} after={err:#x}");
        }
        let px = self.gpu.read_pixels();
        let (w, h) = size();
        write_png(path, w, h, &px).expect("write png");
        println!("wrote {path}");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: headless FILE.pdf OUT_DIR [commands...]");
        std::process::exit(2);
    }
    let (pdf, out) = (&args[1], &args[2]);
    std::fs::create_dir_all(out).unwrap();

    let (w, h) = size();
    let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
    let _egl = Egl::new(w as i32, h as i32).expect("EGL");
    let gpu = GlGpu::new().expect("GL");
    let (tx, rx) = mpsc::channel();
    let sink_tx = std::sync::Mutex::new(tx.clone());
    let sink = std::sync::Arc::new(move |m| {
        let _ = sink_tx.lock().unwrap().send(Event::Worker(m));
    });
    let library = std::path::Path::new(pdf).is_dir();
    let files = std::env::temp_dir().join(if library { "justpdf-headless-lib" } else { "justpdf-headless" });
    if library {
        let _ = std::fs::remove_dir_all(&files);
    }
    std::fs::create_dir_all(&files).unwrap();
    let mut app = App::new(DENSITY, Some(files), sink);
    app.resize(w, h);
    let mock = Mock { log: Vec::new(), tx };
    let mut h = Harness { app, gpu, rx, mock, start: Instant::now(), touch_t: 100.0 };
    let t = h.now();
    h.app.handle(Event::Insets([0.0, 66.0, 0.0, 44.0]), t, &mut h.mock);

    if library {
        let path = std::fs::canonicalize(pdf).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let t = h.now();
        h.app.handle(Event::FolderPicked { uri: format!("file://{}", path.display()), name }, t, &mut h.mock);
        h.settle();
    } else {
        let t = h.now();
        h.mock.open_document(&format!("file://{pdf}"));
        let t0 = Instant::now();
        h.pump();
        while !h.app.has_document() && t0.elapsed() < Duration::from_secs(10) {
            h.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
        println!("opened in {:?} (t={t:.3})", t0.elapsed());
        h.settle();
        println!("first screen settled in {:?}: {}", t0.elapsed(), h.app.stats());
    }

    for cmd in &args[3..] {
        let (op, arg) = cmd.split_once(':').unwrap_or((cmd.as_str(), ""));
        let nums: Vec<f32> = arg.split(',').filter_map(|s| s.parse().ok()).collect();
        match op {
            "shot" => {
                h.settle();
                h.shot(&format!("{out}/{arg}.png"));
            }
            "wait" => h.run_for(arg.parse().unwrap_or(100)),
            "idle" => h.settle(),
            "drag" => h.drag(cx, cy, 0.0, nums[0], 20, 16),
            "dragx" => h.drag(cx, cy, nums[0], 0.0, 20, 16),
            "fling" => h.drag(cx, cy, 0.0, nums[0], 6, 8),
            "swipe" => h.drag(nums[0], nums[1], nums[2], nums[3], 20, 16),
            "tap" => {
                h.tap(nums[0], nums[1]);
                h.run_for(350);
            }
            "dtap" => {
                h.tap(nums[0], nums[1]);
                h.tap(nums[0], nums[1]);
                h.run_for(350);
            }
            "pinch" => {
                let (cx, cy, f) = (nums[0], nums[1], nums[2]);
                let d0 = 150.0;
                h.touch(app::ACTION_DOWN, 0, &[(cx - d0, cy)]);
                h.touch(app::ACTION_POINTER_DOWN, 1, &[(cx - d0, cy), (cx + d0, cy)]);
                for i in 1..=15 {
                    let d = d0 * (1.0 + (f - 1.0) * i as f32 / 15.0);
                    h.touch(app::ACTION_MOVE, 0, &[(cx - d, cy), (cx + d, cy)]);
                }
                let d = d0 * f;
                h.touch(app::ACTION_POINTER_UP, 1, &[(cx - d, cy), (cx + d, cy)]);
                h.touch(app::ACTION_UP, 0, &[(cx - d, cy)]);
            }
            "text" => {
                let t = h.now();
                let n = arg.encode_utf16().count();
                h.app.handle(Event::Text { text: arg.to_string(), cursor: n }, t, &mut h.mock);
            }
            "ime" => {
                let t = h.now();
                h.app.handle(Event::ImeAction, t, &mut h.mock);
            }
            "nothing" => {
                let t = h.now();
                h.app.handle(Event::NothingToOpen, t, &mut h.mock);
            }
            "cancel" => {
                let t = h.now();
                h.app.handle(Event::PickerCancelled, t, &mut h.mock);
            }
            "back" => {
                let t = h.now();
                h.app.handle(Event::Back, t, &mut h.mock);
            }
            "key" => {
                let t = h.now();
                h.app.handle(Event::Key(nums[0] as i32), t, &mut h.mock);
            }
            _ => eprintln!("unknown command {cmd}"),
        }
        let rss =
            std::fs::read_to_string("/proc/self/statm").ok().and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok()).unwrap_or(0)
                * 4
                / 1024;
        println!("  {cmd:<16} -> {:?} redraw={} rss={rss}MB {}", h.app.view(), h.app.needs_redraw(), h.app.stats());
    }
    println!("view: {:?}, mode: {:?}, {}", h.app.view(), h.app.mode(), h.app.stats());
    println!("platform calls: {:?}", h.mock.log);
    h.app.shutdown(Some(&mut h.gpu));
}

fn write_png(path: &str, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, ty: &[u8], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = ty.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    // Raw scanlines (filter 0) wrapped in stored deflate blocks.
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for y in 0..h as usize {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * w as usize * 4..(y + 1) * w as usize * 4]);
    }
    let mut z = vec![0x78, 0x01];
    for (i, block) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        z.push(last as u8);
        z.extend_from_slice(&(block.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &raw {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    std::fs::File::create(path)?.write_all(&png)
}
