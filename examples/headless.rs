//! Renders the real app offscreen (EGL pbuffer + GLES3 via Mesa) and writes
//! PNG screenshots. Used to check rendering and UI without a device.
//!
//! cargo run --release --features headless --example headless -- file.pdf out_dir \
//!     shot:open drag:-2000 wait:500 shot:scrolled tap:540,1200 ...
//!
//! Commands: shot:NAME, wait:MS, drag:DY, dragx:DX, fling:DY, tap:X,Y, dtap:X,Y,
//! pinch:X,Y,FACTOR, text:STRING, ime, back, key:CODE, idle

use std::io::Write;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use justpdf::android_codes;
use justpdf::app::{App, Command, Event, TouchPhase};
use justpdf::egl::Egl;
use justpdf::gl::GlGpu;

const W: u32 = 1080;
const H: u32 = 2340;
const DENSITY: f32 = 2.75;

struct Harness {
    app: App,
    gpu: GlGpu,
    rx: mpsc::Receiver<Event>,
    /// Every platform command the app issued, in order.
    commands: Vec<Command>,
    start: Instant,
    touch_t: f64,
}

impl Harness {
    fn now(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn event(&mut self, ev: Event) {
        let t = self.now();
        self.app.handle(ev, t);
        self.commands.extend(self.app.drain_commands());
    }

    fn update(&mut self) {
        let t = self.now();
        self.app.update(t);
        self.commands.extend(self.app.drain_commands());
    }

    fn pump(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            self.event(ev);
        }
        self.update();
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

    fn touch(&mut self, phase: TouchPhase, index: usize, pts: &[(f32, f32)]) {
        self.touch_t += 0.008;
        let pointers = pts.iter().enumerate().map(|(i, &(x, y))| (i as i32, x, y)).collect();
        self.event(Event::Touch { phase, index, pointers, time: self.touch_t });
        self.update();
        self.app.render(&mut self.gpu);
    }

    fn drag(&mut self, x0: f32, y0: f32, dx: f32, dy: f32, steps: usize, step_ms: u64) {
        self.touch(TouchPhase::Down, 0, &[(x0, y0)]);
        for i in 1..=steps {
            let f = i as f32 / steps as f32;
            std::thread::sleep(Duration::from_millis(step_ms));
            self.touch(TouchPhase::Move, 0, &[(x0 + dx * f, y0 + dy * f)]);
        }
        self.touch(TouchPhase::Up, 0, &[(x0 + dx, y0 + dy)]);
    }

    fn tap(&mut self, x: f32, y: f32) {
        self.touch(TouchPhase::Down, 0, &[(x, y)]);
        self.touch(TouchPhase::Up, 0, &[(x, y)]);
    }

    fn shot(&mut self, path: &str) {
        let before = self.gpu.error();
        self.app.render(&mut self.gpu);
        let err = self.gpu.error();
        if before != 0 || err != 0 {
            eprintln!("GL error before={before:#x} after={err:#x}");
        }
        let px = self.gpu.read_pixels();
        write_png(path, W, H, &px).expect("write png");
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

    let _egl = Egl::new(W as i32, H as i32).expect("EGL");
    let gpu = GlGpu::new().expect("GL");
    let (tx, rx) = mpsc::channel();
    let sink_tx = std::sync::Mutex::new(tx.clone());
    let sink = Box::new(move |m| {
        let _ = sink_tx.lock().unwrap().send(Event::Worker(m));
    });
    let files = std::env::temp_dir().join("justpdf-headless");
    std::fs::create_dir_all(&files).unwrap();
    let mut app = App::new(DENSITY, Some(files), sink);
    app.resize(W, H);
    let mut h = Harness { app, gpu, rx, commands: Vec::new(), start: Instant::now(), touch_t: 100.0 };
    h.event(Event::Insets([0.0, 66.0, 0.0, 44.0]));

    let cpath = std::ffi::CString::new(pdf.as_str()).unwrap();
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    assert!(fd >= 0, "cannot open {pdf}");
    let name = std::path::Path::new(pdf).file_name().unwrap().to_string_lossy().into_owned();
    h.event(Event::Open { fd, uri: format!("file://{pdf}"), name });
    let t0 = Instant::now();
    while !h.app.has_document() && t0.elapsed() < Duration::from_secs(10) {
        h.pump();
        std::thread::sleep(Duration::from_millis(2));
    }
    println!("opened in {:?}", t0.elapsed());
    h.settle();
    println!("first screen settled in {:?}: {}", t0.elapsed(), h.app.stats());

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
            "drag" => h.drag(W as f32 / 2.0, H as f32 / 2.0, 0.0, nums[0], 20, 16),
            "dragx" => h.drag(W as f32 / 2.0, H as f32 / 2.0, nums[0], 0.0, 20, 16),
            "fling" => h.drag(W as f32 / 2.0, H as f32 / 2.0, 0.0, nums[0], 6, 8),
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
                h.touch(TouchPhase::Down, 0, &[(cx - d0, cy)]);
                h.touch(TouchPhase::PointerDown, 1, &[(cx - d0, cy), (cx + d0, cy)]);
                for i in 1..=15 {
                    let d = d0 * (1.0 + (f - 1.0) * i as f32 / 15.0);
                    h.touch(TouchPhase::Move, 0, &[(cx - d, cy), (cx + d, cy)]);
                }
                let d = d0 * f;
                h.touch(TouchPhase::PointerUp, 1, &[(cx - d, cy), (cx + d, cy)]);
                h.touch(TouchPhase::Up, 0, &[(cx - d, cy)]);
            }
            "text" => {
                let n = arg.encode_utf16().count();
                h.event(Event::Text { text: arg.to_string(), cursor: n });
            }
            "ime" => h.event(Event::ImeAction),
            "back" => h.event(Event::Back),
            // Android KeyEvent codes, mapped exactly as on device.
            "key" => match android_codes::key(nums[0] as i32) {
                Some(key) => h.event(Event::Key(key)),
                None => eprintln!("unhandled key code {arg}"),
            },
            _ => eprintln!("unknown command {cmd}"),
        }
        let rss =
            std::fs::read_to_string("/proc/self/statm").ok().and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok()).unwrap_or(0)
                * 4
                / 1024;
        println!("  {cmd:<16} -> {:?} redraw={} rss={rss}MB {}", h.app.view(), h.app.needs_redraw(), h.app.stats());
    }
    println!("view: {:?}, mode: {:?}, {}", h.app.view(), h.app.mode(), h.app.stats());
    println!("platform commands: {:?}", h.commands);
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
