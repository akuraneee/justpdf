//! Application state machine: input, scrolling, tiles, search, overlay.
//! Platform independent; the Android glue feeds it events and a GL backend.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

mod shelf;
use shelf::{Covers, Shelf};

use crate::gesture::{Fling, VelocityTracker};
use crate::gfx::{Gpu, Painter};
use crate::layout::{self, Layout, View, MAX_ZOOM, MIN_ZOOM};
use crate::library::Library;
use crate::pdfium::OutlineItem;
use crate::search;
use crate::store::{Position, Store};
use crate::text::Text;
use crate::tiles::{TileCache, TileKey};
use crate::ui::{self, fade, Icon};
use crate::worker::{Msg, RenderReq, Sink, Worker};

pub const KB_TEXT: i32 = 1;
pub const KB_NUMBER: i32 = 2;
pub const KB_PASSWORD: i32 = 3;

pub const ACTION_DOWN: i32 = 0;
pub const ACTION_UP: i32 = 1;
pub const ACTION_MOVE: i32 = 2;
pub const ACTION_CANCEL: i32 = 3;
pub const ACTION_POINTER_DOWN: i32 = 5;
pub const ACTION_POINTER_UP: i32 = 6;

/// Calls from the app into the host platform.
pub trait Platform {
    /// System document picker; answers with `Event::Open*` or `PickerCancelled`.
    fn open_picker(&mut self);
    /// System folder picker; answers with `Event::FolderPicked` (nothing if cancelled).
    fn open_folder_picker(&mut self);
    /// Lists the PDFs under a library folder; answers with `Event::FolderScanned`.
    fn scan_folder(&mut self, uri: &str);
    /// The folder was removed from the library: give up access to it.
    fn release_folder(&mut self, uri: &str);
    /// Opens a document by URI; answers like the picker (`Event::Open` / `OpenFailed`).
    fn open_document(&mut self, uri: &str);
    /// Opens a document for its cover; answers with `Event::CoverFd`.
    fn open_cover(&mut self, key: u64, uri: &str);
    fn show_keyboard(&mut self, mode: i32, text: &str);
    fn hide_keyboard(&mut self);
    fn system_bars(&mut self, visible: bool);
    /// Keep the display from turning off while the app is in the foreground.
    fn keep_screen_on(&mut self, on: bool);
    fn exit(&mut self);
}

#[derive(Debug)]
pub enum Event {
    /// Safe-area insets in px: left, top, right, bottom.
    Insets([f32; 4]),
    /// Android MotionEvent: masked action, action index, (id, x, y) per pointer, event time in s.
    Touch {
        action: i32,
        index: usize,
        pointers: Vec<(i32, f32, f32)>,
        time: f64,
    },
    Wheel {
        x: f32,
        y: f32,
        delta: f32,
    },
    Key(i32),
    Back,
    Open {
        fd: i32,
        uri: String,
        name: String,
    },
    OpenFailed(String),
    PickerCancelled,
    /// The app started (or a restored document is gone) with nothing to open.
    NothingToOpen,
    /// A folder was chosen to add to the library.
    FolderPicked {
        uri: String,
        name: String,
    },
    /// Result of `Platform::scan_folder`: (folder path, file name, URI) per PDF.
    FolderScanned {
        uri: String,
        books: Vec<(String, String, String)>,
        ok: bool,
    },
    /// Result of `Platform::open_cover`: a read-only fd (owned), or negative.
    CoverFd {
        key: u64,
        fd: i32,
    },
    /// Text field contents from the IME; `cursor` is in UTF-16 units.
    Text {
        text: String,
        cursor: usize,
    },
    ImeAction,
    Pause,
    Worker(Msg),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Read,
    Search,
    Goto,
    Password,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Hit {
    Open,
    Search,
    Chapters,
    Close,
    Prev,
    Next,
    Field,
    Indicator,
    Spread,
    Caffeine,
    Bar,
    ShelfBack,
    ShelfOpenFile,
    ShelfAddFolder,
    ShelfRemove,
    Settings,
    /// The "library at start" row of the settings menu.
    MenuHome,
}

#[derive(Clone, Copy, Debug)]
struct Button {
    r: [f32; 4],
    hit: Hit,
}

fn inside(r: [f32; 4], x: f32, y: f32) -> bool {
    x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]
}

#[derive(Clone, Copy, Debug)]
enum Gesture {
    None,
    Pending {
        x: f32,
        y: f32,
        t: f64,
        stopped: bool,
    },
    Drag {
        id: i32,
        x: f32,
        y: f32,
    },
    Pinch {
        d0: f64,
        z0: f64,
        doc: (f64, f64),
    },
    Button {
        hit: Hit,
        r: [f32; 4],
        inside: bool,
    },
    Scrollbar,
    /// Touch in the contents panel; `x`, `start`: window coordinates where it began.
    Panel {
        x: f32,
        y: f32,
        start: f32,
        moved: bool,
    },
    PanelOutside,
    /// Dragging the docked panel's edge: preview width `w` (px); `grab` is the
    /// finger's offset from the edge when it went down.
    PanelResize {
        w: f32,
        grab: f32,
        moved: bool,
    },
    /// Library grid; `sx`, `sy`: where the touch began.
    Shelf {
        y: f32,
        sx: f32,
        sy: f32,
        moved: bool,
    },
    /// Library tab strip (scrolls sideways); `start`: x where the touch began.
    ShelfTabs {
        x: f32,
        start: f32,
        y: f32,
        moved: bool,
    },
}

struct Anim {
    from: View,
    to: View,
    start: f64,
    dur: f64,
}

struct Doc {
    id: u64,
    uri: String,
    name: String,
    sizes: Vec<(f32, f32)>,
    outline: Vec<OutlineItem>,
    layout: Option<Layout>,
    restore: Option<Position>,
}

struct Pending {
    id: u64,
    uri: String,
    name: String,
    since: f64,
}

struct Match {
    page: u32,
    rects: Vec<[f32; 4]>,
}

#[derive(Default)]
struct SearchState {
    gen: u64,
    query: Vec<char>,
    text: String,
    hits: Vec<Match>,
    current: Option<usize>,
    scanned: u32,
    total: u32,
    running: bool,
    debounce: Option<f64>,
    auto_jump: bool,
}

#[derive(Default)]
struct Panel {
    /// Slide-over drawer (not docked) is open.
    open: bool,
    t: f32,
    scroll: f64,
    fling: Option<Fling>,
    /// Chapter the docked panel last scrolled to.
    follow: Option<usize>,
}

struct Upload {
    doc: u64,
    key: TileKey,
    w: u32,
    h: u32,
    pixels: Vec<u8>,
}

pub struct App {
    density: f32,
    /// Window width; the document viewport is `vw` wide starting at window x `ox`
    /// (non-zero while the contents panel is docked on the left).
    win_w: f32,
    ox: f32,
    vw: f32,
    vh: f32,
    insets: [f32; 4],
    now: f64,
    last_update: f64,
    store: Store,
    library: Library,
    shelf: Shelf,
    covers: Covers,
    worker: Worker,
    next_id: u64,
    doc: Option<Doc>,
    pending: Option<Pending>,

    view: View,
    anim: Option<Anim>,
    fling: Option<Fling>,
    vel: VelocityTracker,
    scroll_dir: f64,
    pointers: Vec<(i32, f32, f32)>,
    gesture: Gesture,
    last_tap: Option<(f64, f32, f32)>,
    tap_at: Option<(f64, f32, f32)>,

    mode: Mode,
    field: String,
    cursor: usize,
    keyboard: bool,
    overlay: bool,
    overlay_t: f32,
    overlay_until: f64,
    activity: f64,
    indicator_t: f32,
    bars: Option<bool>,
    /// Two pages side by side per row.
    spread: bool,
    /// Keep the screen on (and what the platform was last told).
    caffeine: bool,
    caffeine_applied: Option<bool>,
    /// Dock the contents panel at the side when the window is wide enough.
    pin_chapters: bool,
    /// Docked panel width chosen by the user, in dp (0: automatic).
    chapters_w: u32,
    /// Start in the library; off: the system file picker, as before the library existed.
    home: bool,
    /// The settings menu is open.
    menu: bool,
    panel: Panel,
    search: SearchState,
    toast: Option<(String, f64)>,

    tiles: TileCache,
    clear_tiles: bool,
    uploads: VecDeque<Upload>,
    /// Keys sitting in `uploads` (rendered, not yet on the GPU).
    in_flight: HashSet<TileKey>,
    wanted: HashSet<TileKey>,
    sent: Vec<RenderReq>,
    render_level: i8,
    requests_dirty: bool,
    text: Text,
    buttons: Vec<Button>,
    frame: u64,
    dirty: bool,
}

const TAP_TIMEOUT: f64 = 0.25;
const DOUBLE_TAP_GAP: f64 = 0.3;
const OVERLAY_TIMEOUT: f64 = 4.0;

fn ease_out(u: f64) -> f64 {
    1.0 - (1.0 - u).powi(3)
}

fn approach(v: &mut f32, target: f32, step: f32) -> bool {
    if *v == target {
        return false;
    }
    if (*v - target).abs() <= step {
        *v = target;
    } else if *v < target {
        *v += step;
    } else {
        *v -= step;
    }
    true
}

impl App {
    pub fn new(density: f32, files_dir: Option<PathBuf>, sink: Sink) -> App {
        let covers = Covers::new(files_dir.as_ref().map(|d| d.join("covers")));
        let library = Library::load(files_dir.clone());
        let store = Store::load(files_dir);
        let shelf = Shelf::new(&library, store.shelf());
        let spread = store.spread();
        let caffeine = store.caffeine();
        let pin_chapters = store.pin_chapters();
        let chapters_w = store.chapters_w();
        let home = store.home();
        App {
            density: density.max(0.5),
            win_w: 0.0,
            ox: 0.0,
            vw: 0.0,
            vh: 0.0,
            insets: [0.0; 4],
            now: 0.0,
            last_update: 0.0,
            store,
            library,
            shelf,
            covers,
            worker: Worker::new(sink),
            next_id: 1,
            doc: None,
            pending: None,
            view: View::default(),
            anim: None,
            fling: None,
            vel: VelocityTracker::default(),
            scroll_dir: 1.0,
            pointers: Vec::new(),
            gesture: Gesture::None,
            last_tap: None,
            tap_at: None,
            mode: Mode::Read,
            field: String::new(),
            cursor: 0,
            keyboard: false,
            overlay: false,
            overlay_t: 0.0,
            overlay_until: 0.0,
            activity: -10.0,
            indicator_t: 0.0,
            bars: None,
            spread,
            caffeine,
            caffeine_applied: None,
            pin_chapters,
            chapters_w,
            home,
            menu: false,
            panel: Panel::default(),
            search: SearchState::default(),
            toast: None,
            tiles: TileCache::new(96 << 20),
            clear_tiles: false,
            uploads: VecDeque::new(),
            in_flight: HashSet::new(),
            wanted: HashSet::new(),
            sent: Vec::new(),
            render_level: 0,
            requests_dirty: false,
            text: Text::new(),
            buttons: Vec::new(),
            frame: 0,
            dirty: true,
        }
    }

    fn dp(&self, v: f32) -> f32 {
        v * self.density
    }

    // ----- queries used by the platform loop -------------------------------------------

    pub fn has_document(&self) -> bool {
        self.doc.is_some()
    }

    pub fn current_uri(&self) -> Option<&str> {
        self.doc.as_ref().map(|d| d.uri.as_str())
    }

    /// Cache statistics for diagnostics.
    pub fn stats(&self) -> String {
        format!(
            "tiles={} bytes={}MB uploads={} queued={} wanted={} level={}",
            self.tiles.len(),
            self.tiles.bytes() >> 20,
            self.uploads.len(),
            self.sent.len(),
            self.wanted.len(),
            self.render_level
        )
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn view(&self) -> View {
        self.view
    }

    /// Whether a back press would be handled by the app (rather than exiting).
    pub fn back_consumable(&self) -> bool {
        self.menu || self.panel.open || self.mode != Mode::Read || (self.shelf.open && self.doc.is_some())
    }

    /// The library screen is up: over a document, or as the home screen without one.
    pub fn shelf_visible(&self) -> bool {
        self.shelf.open || (self.doc.is_none() && self.home)
    }

    /// Nothing is open or being opened and the library is off: the old flow,
    /// where the app lives in the system file picker.
    fn picker_home(&self) -> bool {
        !self.home && self.doc.is_none() && self.pending.is_none()
    }

    /// Something is moving: render continuously.
    pub fn animating(&self) -> bool {
        self.fling.is_some()
            || self.anim.is_some()
            || self.panel.fling.is_some()
            || self.overlay_t != self.overlay_target()
            || self.indicator_t != self.indicator_target()
            || self.panel.t != if self.panel.open { 1.0 } else { 0.0 }
            || !self.uploads.is_empty()
            || self.shelf.fling.is_some()
            || (self.shelf_visible() && self.covers.uploading())
    }

    pub fn needs_redraw(&self) -> bool {
        self.dirty || self.animating()
    }

    /// Next time `update` must run even without events.
    pub fn next_deadline(&self) -> Option<f64> {
        let mut t: Option<f64> = None;
        let mut add = |v: f64| t = Some(t.map_or(v, |c: f64| c.min(v)));
        if let Some((at, _, _)) = self.tap_at {
            add(at);
        }
        if self.overlay && self.mode == Mode::Read && !self.panel.open && !self.menu {
            add(self.overlay_until);
        }
        if self.indicator_t > 0.0 {
            add(self.activity + 1.0);
        }
        if let Some((_, until)) = &self.toast {
            add(*until);
        }
        if let Some(d) = self.search.debounce {
            add(d);
        }
        if let Some((_, since)) = &self.shelf.opening {
            add(since + 0.3);
        }
        t
    }

    // ----- geometry ---------------------------------------------------------------------

    fn bar_h(&self) -> f32 {
        self.dp(56.0)
    }

    fn bar_bottom(&self) -> f32 {
        self.insets[1] + self.bar_h()
    }

    /// Width of the contents panel: the dock while docked, else the drawer.
    fn panel_w(&self) -> f32 {
        if self.docked() {
            match self.gesture {
                Gesture::PanelResize { w, moved: true, .. } => w,
                _ => self.ox,
            }
        } else {
            self.dp(360.0).min(self.win_w * 0.86)
        }
    }

    fn docked(&self) -> bool {
        self.ox > 0.0
    }

    /// Width the docked panel should have now, if it should be docked at all:
    /// pinned, the document has chapters and the window leaves room to read.
    fn dock_w(&self) -> Option<f32> {
        if !self.pin_chapters || !self.doc.as_ref().is_some_and(|d| !d.outline.is_empty()) {
            return None;
        }
        let (lo, hi) = self.dock_range()?;
        let want =
            if self.chapters_w > 0 { self.dp(self.chapters_w as f32) } else { (self.win_w * 0.32).clamp(self.dp(220.0), self.dp(340.0)) };
        Some(want.clamp(lo, hi).round())
    }

    /// Widths (px) the docked panel may take in this window: at least 220 dp,
    /// leaving at least 360 dp (and 40% of the window) for the document.
    fn dock_range(&self) -> Option<(f32, f32)> {
        let lo = self.dp(220.0);
        let hi = (self.win_w - self.dp(360.0)).min(self.win_w * 0.6);
        (hi >= lo).then_some((lo, hi))
    }

    /// Pin button in the panel header (window coordinates, panel fully shown).
    fn pin_rect(&self) -> [f32; 4] {
        let b = self.dp(48.0);
        [self.panel_w() - b - self.dp(4.0), self.insets[1] + self.dp(28.0) - b / 2.0, b, b]
    }

    fn panel_top(&self) -> f32 {
        self.insets[1] + self.dp(56.0)
    }

    fn row_h(&self) -> f64 {
        self.dp(48.0) as f64
    }

    fn panel_max_scroll(&self) -> f64 {
        let n = self.doc.as_ref().map_or(0, |d| d.outline.len()) as f64;
        (n * self.row_h() - (self.vh - self.insets[3] - self.panel_top()) as f64).max(0.0)
    }

    fn overlay_target(&self) -> f32 {
        if self.overlay_visible() {
            1.0
        } else {
            0.0
        }
    }

    fn overlay_visible(&self) -> bool {
        self.overlay || self.menu || self.mode != Mode::Read || self.shelf_visible()
    }

    fn indicator_target(&self) -> f32 {
        if self.now - self.activity < 1.0 {
            1.0
        } else {
            0.0
        }
    }

    fn layout(&self) -> Option<&Layout> {
        self.doc.as_ref().and_then(|d| d.layout.as_ref())
    }

    fn current_page(&self) -> usize {
        match self.layout() {
            Some(l) => l.page_at(self.view.y + self.vh as f64 / self.view.zoom / 2.0),
            None => 0,
        }
    }

    fn position(&self) -> Option<Position> {
        let l = self.layout()?;
        let p = l.page_at(self.view.y.max(0.0));
        Some(Position {
            page: p as u32,
            frac: ((self.view.y - l.tops[p]) / l.heights[p]).clamp(0.0, 1.0),
            zoom: self.view.zoom,
            x: self.view.x / l.width,
        })
    }

    fn clamp_view(&self, v: View) -> View {
        match self.layout() {
            Some(l) => v.clamped(l, self.vw as f64, self.vh as f64),
            None => v,
        }
    }

    fn set_view(&mut self, v: View) {
        let v = self.clamp_view(v);
        if v != self.view {
            self.view = v;
            self.view_changed();
        }
    }

    fn view_changed(&mut self) {
        self.requests_dirty = true;
        self.dirty = true;
    }

    fn touch_activity(&mut self) {
        self.activity = self.now;
    }

    fn animate_to(&mut self, to: View, dur: f64) {
        let to = self.clamp_view(to);
        self.fling = None;
        self.anim = Some(Anim { from: self.view, to, start: self.now, dur });
        self.dirty = true;
    }

    // ----- lifecycle --------------------------------------------------------------------

    pub fn resize(&mut self, w: u32, h: u32) {
        let (w, h) = (w.max(1) as f32, h.max(1) as f32);
        if w == self.win_w && h == self.vh {
            return;
        }
        self.win_w = w;
        let screen = (w * h * 4.0) as usize;
        self.tiles.budget = (screen * 8).clamp(96 << 20, 256 << 20);
        self.update_dock(h);
    }

    /// Docks or undocks the contents panel as the preference, the document and
    /// the window width require, resizing the document viewport to match.
    fn update_dock(&mut self, h: f32) {
        if self.win_w <= 0.0 {
            return;
        }
        let was = self.docked();
        self.ox = self.dock_w().unwrap_or(0.0);
        self.set_viewport(self.win_w - self.ox, h);
        if self.docked() && !was {
            self.panel.open = false;
            self.panel.t = 0.0;
            self.panel.fling = None;
            self.panel.follow = self.current_chapter();
            if let Some(c) = self.panel.follow {
                self.center_chapter(c);
            }
        }
        self.dirty = true;
    }

    /// Sets the document viewport size (px), rebuilding the layout on width changes.
    fn set_viewport(&mut self, w: f32, h: f32) {
        if w == self.vw && h == self.vh {
            return;
        }
        let width_changed = w != self.vw;
        let pos = self.position();
        self.vw = w;
        self.vh = h;
        if width_changed {
            self.clear_tiles = true;
            if let Some(d) = self.doc.as_mut() {
                if d.layout.is_some() {
                    d.layout = None;
                    d.restore = pos;
                }
            }
        }
        self.ensure_layout();
        self.view = self.clamp_view(self.view);
        self.view_changed();
    }

    fn ensure_layout(&mut self) {
        if self.vw <= 0.0 {
            return;
        }
        let Some(d) = self.doc.as_mut() else { return };
        if d.layout.is_some() {
            return;
        }
        let l = Layout::new(&d.sizes, self.vw as f64, self.spread);
        let view = match d.restore.take() {
            Some(p) => View { zoom: p.zoom.clamp(MIN_ZOOM, MAX_ZOOM), x: p.x * l.width, y: l.y_of(p.page as usize, p.frac) },
            None => View::default(),
        };
        d.layout = Some(l);
        self.anim = None;
        self.fling = None;
        self.view = self.clamp_view(view);
        self.render_level = layout::level_for_zoom(self.view.zoom);
        self.view_changed();
    }

    pub fn save_position(&mut self) {
        if let Some(pos) = self.position() {
            if let Some(d) = &self.doc {
                let uri = d.uri.clone();
                self.store.set(&uri, pos);
            }
        }
        self.store.save();
    }

    pub fn shutdown(&mut self, gpu: Option<&mut dyn Gpu>) {
        self.save_position();
        self.worker.close();
        if let Some(gpu) = gpu {
            self.tiles.clear(gpu);
        }
    }

    /// Forces a redraw and a fresh tile request (e.g. after the window came back).
    pub fn invalidate(&mut self) {
        self.dirty = true;
        self.requests_dirty = true;
    }

    /// The window went away: keep only what is on screen.
    pub fn trim(&mut self, gpu: &mut dyn Gpu) {
        self.covers.clear_gpu(gpu);
        let Some(l) = self.layout() else { return };
        let v = self.view;
        let range = l.range(v.y, v.y + self.vh as f64 / v.zoom);
        let level = self.render_level;
        self.tiles.retain(gpu, |k| range.contains(&(k.page as usize)) && (k.level == level || k.is_thumb()));
    }

    /// The GL context was lost; every texture is gone.
    pub fn gpu_lost(&mut self) {
        self.tiles.forget();
        self.covers.forget_gpu();
        self.text.forget_gpu();
        self.requests_dirty = true;
        self.dirty = true;
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), self.now + 2.5));
        self.dirty = true;
    }

    // ----- events -----------------------------------------------------------------------

    pub fn handle(&mut self, ev: Event, now: f64, p: &mut dyn Platform) {
        self.now = now;
        self.dirty = true;
        match ev {
            Event::Insets(i) => self.insets = i,
            Event::Touch { action, index, pointers, time } => self.touch(action, index, pointers, time, p),
            Event::Wheel { delta, .. } if self.shelf_visible() => self.shelf_scroll_by(-(delta as f64) * self.dp(64.0) as f64),
            Event::Wheel { delta, .. } => {
                let dy = -(delta as f64) * self.dp(64.0) as f64 / self.view.zoom;
                self.anim = None;
                self.fling = None;
                self.scroll_dir = if dy >= 0.0 { 1.0 } else { -1.0 };
                self.set_view(View { y: self.view.y + dy, ..self.view });
                self.touch_activity();
            }
            Event::Key(_) if self.shelf_visible() => {}
            Event::Key(code) => self.key(code),
            Event::Back => {
                self.back(p);
            }
            Event::Open { fd, uri, name } => {
                self.save_position();
                let id = self.next_id;
                self.next_id += 1;
                self.worker.open(id, fd);
                if self.mode == Mode::Password {
                    self.set_mode(Mode::Read, p);
                }
                self.pending = Some(Pending { id, uri, name, since: now });
            }
            Event::OpenFailed(msg) => {
                crate::log_w!("open failed: {msg}");
                self.shelf.opening = None;
                self.toast("Can't open this file");
                if self.picker_home() {
                    p.open_picker();
                }
            }
            Event::PickerCancelled => {
                if self.picker_home() {
                    p.exit();
                }
            }
            Event::NothingToOpen => {
                if self.picker_home() {
                    p.open_picker();
                }
            }
            Event::FolderPicked { uri, name } => self.folder_picked(&uri, &name, p),
            Event::FolderScanned { uri, books, ok } => self.folder_scanned(&uri, books, ok),
            Event::CoverFd { key, fd } => self.cover_fd(key, fd),
            Event::Text { text, cursor } => {
                self.cursor = utf16_to_char_index(&text, cursor);
                if text != self.field {
                    self.field = text;
                    if self.mode == Mode::Search {
                        self.search.debounce = Some(now + 0.25);
                    }
                }
            }
            Event::ImeAction => self.ime_action(p),
            Event::Pause => self.save_position(),
            Event::Worker(msg) => self.worker_msg(msg, p),
        }
    }

    fn worker_msg(&mut self, msg: Msg, p: &mut dyn Platform) {
        match msg {
            Msg::Opened { doc, sizes } => {
                let Some(pending) = self.pending.take_if(|pd| pd.id == doc) else { return };
                let restore = self.store.get(&pending.uri);
                self.store.opened(&pending.uri, &pending.name, sizes.len() as u32);
                self.opened_cover(doc, &pending.uri);
                self.shelf.open = false;
                self.shelf.opening = None;
                self.shelf.stale = true;
                self.doc = Some(Doc { id: doc, uri: pending.uri, name: pending.name, sizes, outline: Vec::new(), layout: None, restore });
                self.clear_tiles = true;
                self.sent.clear();
                self.wanted.clear();
                self.search = SearchState::default();
                self.panel = Panel::default();
                if self.mode != Mode::Read {
                    self.set_mode(Mode::Read, p);
                }
                self.view = View::default();
                self.ensure_layout();
                self.overlay = true;
                self.overlay_until = self.now + OVERLAY_TIMEOUT;
                self.touch_activity();
            }
            Msg::NeedPassword { doc, wrong } => {
                if self.pending.as_ref().is_some_and(|pd| pd.id == doc) {
                    self.field.clear();
                    self.cursor = 0;
                    self.mode = Mode::Password;
                    p.show_keyboard(KB_PASSWORD, "");
                    self.keyboard = true;
                    if wrong {
                        self.toast("Wrong password");
                    }
                }
            }
            Msg::OpenFailed { doc, error } => {
                if self.pending.take_if(|pd| pd.id == doc).is_some() {
                    crate::log_w!("cannot open: {error}");
                    self.shelf.opening = None;
                    self.toast("Can't open this file");
                    if self.picker_home() {
                        p.open_picker();
                    }
                }
            }
            Msg::Outline { doc, items } => {
                if let Some(d) = self.doc.as_mut().filter(|d| d.id == doc) {
                    d.outline = items;
                    self.update_dock(self.vh);
                }
            }
            Msg::Tile { doc, key, w, h, pixels } => {
                if self.doc.as_ref().is_some_and(|d| d.id == doc) && !self.tiles.contains(&key) && self.in_flight.insert(key) {
                    self.uploads.push_back(Upload { doc, key, w, h, pixels });
                } else {
                    self.worker.recycle(pixels);
                }
            }
            Msg::SearchHits { doc, gen, page, hits } => {
                if self.doc.as_ref().is_some_and(|d| d.id == doc) && gen == self.search.gen {
                    self.add_hits(page, hits);
                }
            }
            Msg::SearchProgress { doc, gen, scanned, total } => {
                if self.doc.as_ref().is_some_and(|d| d.id == doc) && gen == self.search.gen {
                    self.search.scanned = scanned;
                    self.search.total = total;
                    if scanned >= total {
                        self.search.running = false;
                    }
                }
            }
            Msg::Cover { key, w, h, pixels } => self.cover_ready(key, w, h, pixels),
            Msg::CoverFailed { key, missing } => self.cover_failed(key, missing),
        }
    }

    fn key(&mut self, code: i32) {
        let vh = self.vh as f64 / self.view.zoom;
        let vw = self.vw as f64 / self.view.zoom;
        let v = self.view;
        let to = match code {
            20 => View { y: v.y + vh * 0.15, ..v },
            19 => View { y: v.y - vh * 0.15, ..v },
            93 | 62 => View { y: v.y + vh * 0.85, ..v },
            92 => View { y: v.y - vh * 0.85, ..v },
            122 => View { y: 0.0, ..v },
            123 => View { y: f64::MAX / 4.0, ..v },
            21 => View { x: v.x - vw * 0.2, ..v },
            22 => View { x: v.x + vw * 0.2, ..v },
            _ => return,
        };
        self.scroll_dir = if to.y >= v.y { 1.0 } else { -1.0 };
        self.animate_to(to, 0.18);
        self.touch_activity();
    }

    fn back(&mut self, p: &mut dyn Platform) -> bool {
        if self.menu {
            self.menu = false;
            return true;
        }
        if self.panel.open {
            self.panel.open = false;
            return true;
        }
        if self.mode != Mode::Read {
            self.set_mode(Mode::Read, p);
            return true;
        }
        if self.shelf.open && self.doc.is_some() {
            self.hide_shelf();
            return true;
        }
        false
    }

    fn set_mode(&mut self, mode: Mode, p: &mut dyn Platform) {
        let old = self.mode;
        if old == Mode::Search && mode != Mode::Search {
            self.search.text = self.field.clone();
            self.worker.cancel_search();
            let gen = self.search.gen + 1;
            let text = std::mem::take(&mut self.search.text);
            self.search = SearchState { gen, text, ..Default::default() };
        }
        if old == Mode::Password && mode != Mode::Password {
            self.pending = None;
            self.shelf.opening = None;
            if self.picker_home() {
                p.open_picker();
            }
        }
        self.mode = mode;
        self.panel.open = false;
        self.menu = false;
        match mode {
            Mode::Read => {
                if self.keyboard {
                    p.hide_keyboard();
                    self.keyboard = false;
                }
                self.overlay = true;
                self.overlay_until = self.now + OVERLAY_TIMEOUT;
            }
            Mode::Search => {
                self.field = self.search.text.clone();
                self.cursor = self.field.chars().count();
                p.show_keyboard(KB_TEXT, &self.field);
                self.keyboard = true;
                if !self.field.is_empty() {
                    self.search.query.clear();
                    self.start_search();
                }
            }
            Mode::Goto => {
                self.field.clear();
                self.cursor = 0;
                p.show_keyboard(KB_NUMBER, "");
                self.keyboard = true;
            }
            Mode::Password => {}
        }
    }

    fn ime_action(&mut self, p: &mut dyn Platform) {
        match self.mode {
            Mode::Search => {
                if self.search.debounce.is_some() || search::fold_query(&self.field) != self.search.query {
                    self.start_search();
                } else {
                    self.step_match(1);
                }
                p.hide_keyboard();
                self.keyboard = false;
            }
            Mode::Goto => {
                let n = self.doc.as_ref().map_or(0, |d| d.sizes.len());
                match self.field.trim().parse::<usize>() {
                    Ok(page) if page >= 1 && page <= n => {
                        self.jump_to(page - 1, 0.0);
                        self.set_mode(Mode::Read, p);
                        self.overlay = false;
                    }
                    _ => self.toast(format!("Enter a page from 1 to {n}")),
                }
            }
            Mode::Password => {
                if let Some(pd) = &self.pending {
                    self.worker.password(pd.id, self.field.clone());
                    p.hide_keyboard();
                    self.keyboard = false;
                }
            }
            Mode::Read => {}
        }
    }

    fn jump_to(&mut self, page: usize, frac: f64) {
        let Some(l) = self.layout() else { return };
        let y = l.y_of(page, frac);
        self.anim = None;
        self.fling = None;
        self.set_view(View { y, ..self.view });
        self.touch_activity();
    }

    // ----- search -----------------------------------------------------------------------

    fn start_search(&mut self) {
        self.search.debounce = None;
        let q = search::fold_query(&self.field);
        if q == self.search.query && !self.search.hits.is_empty() {
            return;
        }
        self.search.gen += 1;
        self.search.query = q.clone();
        self.search.hits.clear();
        self.search.current = None;
        self.search.scanned = 0;
        self.search.auto_jump = true;
        let (id, n) = match &self.doc {
            Some(d) => (d.id, d.sizes.len() as u32),
            None => return,
        };
        self.search.total = n;
        self.search.running = !q.is_empty();
        let start = self.current_page() as u32;
        self.worker.search(id, self.search.gen, q, start, n);
    }

    fn add_hits(&mut self, page: u32, hits: Vec<Vec<[f32; 4]>>) {
        let idx = self.search.hits.partition_point(|m| m.page < page);
        let n = hits.len();
        let new = hits.into_iter().map(|rects| Match { page, rects });
        self.search.hits.splice(idx..idx, new);
        match self.search.current {
            Some(c) if c >= idx => self.search.current = Some(c + n),
            Some(_) => {}
            None => {
                self.search.current = Some(idx);
                if self.search.auto_jump {
                    self.search.auto_jump = false;
                    self.go_to_match(idx);
                }
            }
        }
    }

    fn step_match(&mut self, dir: i64) {
        let n = self.search.hits.len() as i64;
        if n == 0 {
            return;
        }
        let c = self.search.current.map_or(0, |c| (c as i64 + dir).rem_euclid(n)) as usize;
        self.search.current = Some(c);
        self.search.auto_jump = false;
        self.go_to_match(c);
    }

    fn go_to_match(&mut self, i: usize) {
        let Some(m) = self.search.hits.get(i) else { return };
        let Some(l) = self.layout() else { return };
        let page = m.page as usize;
        if page >= l.len() {
            return;
        }
        let r = m.rects.iter().fold([1f32, 1.0, 0.0, 0.0], |a, r| [a[0].min(r[0]), a[1].min(r[1]), a[2].max(r[2]), a[3].max(r[3])]);
        let doc_y = l.tops[page] + r[1] as f64 * l.heights[page];
        let doc_x = l.xs[page] + (r[0] + r[2]) as f64 / 2.0 * l.widths[page];
        let z = self.view.zoom;
        let (vw, vh) = (self.vw as f64 / z, self.vh as f64 / z);
        let top = self.bar_bottom() as f64 / z;
        let mut to = self.view;
        // Only scroll if the match is not comfortably visible.
        if doc_y < self.view.y + top || doc_y > self.view.y + vh * 0.8 {
            to.y = doc_y - vh * 0.3;
        }
        if z > 1.0 && (doc_x < self.view.x || doc_x > self.view.x + vw) {
            to.x = doc_x - vw / 2.0;
        }
        let to = self.clamp_view(to);
        if (to.y - self.view.y).abs() * z < self.vh as f64 * 2.5 {
            self.animate_to(to, 0.25);
        } else {
            self.anim = None;
            self.set_view(to);
        }
        self.touch_activity();
    }

    // ----- touch ------------------------------------------------------------------------

    fn touch(&mut self, action: i32, index: usize, pointers: Vec<(i32, f32, f32)>, time: f64, p: &mut dyn Platform) {
        self.pointers = pointers;
        if self.menu {
            return self.menu_touch(action, index, p);
        }
        if self.shelf_visible() && self.mode != Mode::Password {
            // The library works in window coordinates.
            return self.shelf_touch(action, index, time, p);
        }
        // Everything but the docked panel works in document-viewport coordinates.
        if self.ox != 0.0 {
            for p in &mut self.pointers {
                p.1 -= self.ox;
            }
        }
        let (px, py) = self.pointers.get(index).map_or((0.0, 0.0), |&(_, x, y)| (x, y));
        match action {
            ACTION_DOWN => self.on_down(px, py, time),
            ACTION_POINTER_DOWN => self.on_pointer_down(),
            ACTION_MOVE => self.on_move(time),
            ACTION_POINTER_UP => self.on_pointer_up(index),
            ACTION_UP => self.on_up(px, py, time, p),
            ACTION_CANCEL => {
                self.gesture = Gesture::None;
                self.vel.reset();
            }
            _ => {}
        }
    }

    fn on_down(&mut self, x: f32, y: f32, time: f64) {
        self.vel.reset();
        self.vel.add(time, x as f64, y as f64);
        let wx = x + self.ox;
        // Grab zone around the docked panel's edge (below the top bar).
        if self.docked() && y > self.panel_top() && wx > self.ox - self.dp(10.0) && wx < self.ox + self.dp(14.0) {
            self.panel.fling = None;
            self.gesture = Gesture::PanelResize { w: self.ox, grab: wx - self.ox, moved: false };
            return;
        }
        if self.panel.open || (self.docked() && wx < self.ox) {
            self.panel.fling = None;
            let on_panel = wx < self.panel_w();
            self.gesture = if on_panel { Gesture::Panel { x: wx, y, start: y, moved: false } } else { Gesture::PanelOutside };
            return;
        }
        let controls = self.overlay_t > 0.5 || self.mode != Mode::Read;
        if controls {
            if let Some(b) = self.buttons.iter().rev().find(|b| inside(b.r, x, y)).copied() {
                self.gesture = Gesture::Button { hit: b.hit, r: b.r, inside: true };
                return;
            }
        }
        let scroll_alpha = self.overlay_t.max(self.indicator_t);
        if scroll_alpha > 0.3 && x > self.vw - self.insets[2] - self.dp(36.0) && self.scrollable() {
            self.gesture = Gesture::Scrollbar;
            self.fling = None;
            self.anim = None;
            self.scrollbar_to(y);
            return;
        }
        let stopped = self.fling.is_some() || self.anim.is_some();
        self.fling = None;
        self.anim = None;
        self.gesture = Gesture::Pending { x, y, t: time, stopped };
    }

    fn on_pointer_down(&mut self) {
        if self.pointers.len() < 2 {
            return;
        }
        match self.gesture {
            Gesture::Pending { .. } | Gesture::Drag { .. } | Gesture::Pinch { .. } => {
                let (_, x0, y0) = self.pointers[0];
                let (_, x1, y1) = self.pointers[1];
                let d0 = ((x1 - x0) as f64).hypot((y1 - y0) as f64).max(1.0);
                let mid = ((x0 + x1) as f64 / 2.0, (y0 + y1) as f64 / 2.0);
                self.gesture = Gesture::Pinch { d0, z0: self.view.zoom, doc: self.view.to_doc(mid.0, mid.1) };
                self.tap_at = None;
                self.fling = None;
                self.anim = None;
            }
            _ => {}
        }
    }

    fn on_pointer_up(&mut self, index: usize) {
        if let Gesture::Pinch { .. } | Gesture::Drag { .. } = self.gesture {
            // Continue as a drag with a remaining finger.
            if let Some(&(id, x, y)) = self.pointers.iter().enumerate().find(|(i, _)| *i != index).map(|(_, p)| p) {
                self.gesture = Gesture::Drag { id, x, y };
                self.vel.reset();
            }
            self.requests_dirty = true;
        }
    }

    fn on_move(&mut self, time: f64) {
        let slop = self.dp(8.0);
        match self.gesture {
            Gesture::Pending { x, y, .. } => {
                let Some(&(id, cx, cy)) = self.pointers.first() else { return };
                self.vel.add(time, cx as f64, cy as f64);
                if (cx - x).hypot(cy - y) > slop {
                    self.gesture = Gesture::Drag { id, x: cx, y: cy };
                    if self.mode == Mode::Read && self.overlay {
                        self.overlay = false;
                    }
                    self.tap_at = None;
                    // Apply the movement past the slop immediately.
                    self.drag_by(cx - x, cy - y);
                }
            }
            Gesture::Drag { id, x, y } => {
                let Some(&(_, cx, cy)) = self.pointers.iter().find(|p| p.0 == id) else { return };
                self.vel.add(time, cx as f64, cy as f64);
                self.gesture = Gesture::Drag { id, x: cx, y: cy };
                self.drag_by(cx - x, cy - y);
            }
            Gesture::Pinch { d0, z0, doc } => {
                if self.pointers.len() < 2 {
                    return;
                }
                let (_, x0, y0) = self.pointers[0];
                let (_, x1, y1) = self.pointers[1];
                let d = ((x1 - x0) as f64).hypot((y1 - y0) as f64).max(1.0);
                let (mx, my) = ((x0 + x1) as f64 / 2.0, (y0 + y1) as f64 / 2.0);
                let z = (z0 * d / d0).clamp(MIN_ZOOM, MAX_ZOOM);
                self.set_view(View { zoom: z, x: doc.0 - mx / z, y: doc.1 - my / z });
                self.touch_activity();
            }
            Gesture::Button { hit, r, .. } => {
                if let Some(&(_, cx, cy)) = self.pointers.first() {
                    self.gesture = Gesture::Button { hit, r, inside: inside(r, cx, cy) };
                }
            }
            Gesture::Scrollbar => {
                if let Some(&(_, _, cy)) = self.pointers.first() {
                    self.scrollbar_to(cy);
                }
            }
            Gesture::Panel { x, y, start, moved } => {
                let Some(&(_, _, cy)) = self.pointers.first() else { return };
                self.vel.add(time, 0.0, cy as f64);
                let moved = moved || (cy - start).abs() > slop;
                if moved {
                    let max = self.panel_max_scroll();
                    self.panel.scroll = (self.panel.scroll - (cy - y) as f64).clamp(0.0, max);
                }
                self.gesture = Gesture::Panel { x, y: cy, start, moved };
            }
            Gesture::PanelResize { w, grab, moved } => {
                let Some(&(_, cx, _)) = self.pointers.first() else { return };
                let edge = cx + self.ox - grab;
                let moved = moved || (edge - self.ox).abs() > slop;
                let w = match self.dock_range() {
                    Some((lo, hi)) if moved => edge.clamp(lo, hi),
                    _ => w,
                };
                self.gesture = Gesture::PanelResize { w, grab, moved };
            }
            Gesture::PanelOutside | Gesture::Shelf { .. } | Gesture::ShelfTabs { .. } | Gesture::None => {}
        }
    }

    fn drag_by(&mut self, dx: f32, dy: f32) {
        let z = self.view.zoom;
        if dy != 0.0 {
            self.scroll_dir = if dy < 0.0 { 1.0 } else { -1.0 };
        }
        self.set_view(View { x: self.view.x - dx as f64 / z, y: self.view.y - dy as f64 / z, ..self.view });
        self.touch_activity();
    }

    fn on_up(&mut self, x: f32, y: f32, time: f64, p: &mut dyn Platform) {
        let gesture = std::mem::replace(&mut self.gesture, Gesture::None);
        match gesture {
            Gesture::Pending { t, stopped, .. } => {
                if stopped {
                    return;
                }
                let dbl = self.last_tap.is_some_and(|(lt, lx, ly)| t - lt < DOUBLE_TAP_GAP && (x - lx).hypot(y - ly) < self.dp(40.0));
                if dbl {
                    self.last_tap = None;
                    self.tap_at = None;
                    self.double_tap(x, y);
                } else {
                    self.last_tap = Some((time, x, y));
                    self.tap_at = Some((self.now + TAP_TIMEOUT, x, y));
                }
            }
            Gesture::Drag { .. } => {
                let (vx, vy) = self.vel.velocity(time);
                let (vx, vy) = if self.view.zoom > 1.0001 { (vx, vy) } else { (0.0, vy) };
                self.fling = Fling::new(self.now, vx, vy, self.density as f64);
            }
            Gesture::Pinch { .. } => {
                self.requests_dirty = true;
            }
            Gesture::Button { hit, r, .. } => {
                if inside(r, x, y) {
                    self.press(hit, p);
                }
            }
            Gesture::Scrollbar => {}
            Gesture::Panel { x: sx, start, moved, .. } => {
                if moved {
                    let (_, vy) = self.vel.velocity(time);
                    self.panel.fling = Fling::new(self.now, 0.0, vy, self.density as f64);
                } else if start < self.panel_top() {
                    let r = self.pin_rect();
                    if inside(r, sx, start) && inside(r, x + self.ox, y) {
                        self.toggle_pin();
                    }
                } else {
                    let row = ((y - self.panel_top()) as f64 + self.panel.scroll) / self.row_h();
                    if row >= 0.0 && y >= self.panel_top() {
                        let item = self.doc.as_ref().and_then(|d| d.outline.get(row as usize)).cloned();
                        if let Some(item) = item {
                            self.jump_to(item.page as usize, item.y.unwrap_or(0.0) as f64);
                            self.panel.open = false;
                            self.overlay = false;
                        }
                    }
                }
            }
            Gesture::PanelOutside => self.panel.open = false,
            Gesture::PanelResize { w, moved, .. } => {
                if moved {
                    self.chapters_w = (w / self.density).round() as u32;
                    self.store.set_chapters_w(self.chapters_w);
                    self.update_dock(self.vh);
                }
            }
            Gesture::Shelf { .. } | Gesture::ShelfTabs { .. } | Gesture::None => {}
        }
    }

    fn single_tap(&mut self, _x: f32, _y: f32, p: &mut dyn Platform) {
        if self.keyboard && self.mode != Mode::Read {
            p.hide_keyboard();
            self.keyboard = false;
            return;
        }
        if self.mode == Mode::Read && self.doc.is_some() {
            self.overlay = !self.overlay;
            self.overlay_until = self.now + OVERLAY_TIMEOUT;
            self.dirty = true;
        }
    }

    fn double_tap(&mut self, x: f32, y: f32) {
        let target = if self.view.zoom > 1.05 || self.view.zoom < 0.95 { 1.0 } else { 2.5 };
        let to = self.view.zoomed_at(target, x as f64, y as f64);
        self.animate_to(to, 0.22);
    }

    fn press(&mut self, hit: Hit, p: &mut dyn Platform) {
        self.overlay_until = self.now + OVERLAY_TIMEOUT;
        match hit {
            Hit::Open if self.home => self.show_shelf(p),
            Hit::Open => p.open_picker(),
            Hit::Settings => self.menu = !self.menu,
            Hit::MenuHome => {
                self.home = !self.home;
                self.store.set_home(self.home);
                self.invalidate();
            }
            Hit::ShelfBack | Hit::ShelfOpenFile | Hit::ShelfAddFolder | Hit::ShelfRemove => self.shelf_press(hit, p),
            Hit::Search => self.set_mode(Mode::Search, p),
            Hit::Chapters => {
                if self.docked() {
                    self.toggle_pin();
                } else {
                    self.open_panel();
                }
            }
            Hit::Spread => self.toggle_spread(),
            Hit::Caffeine => {
                self.caffeine = !self.caffeine;
                self.store.set_caffeine(self.caffeine);
                self.toast(if self.caffeine { "Screen stays on" } else { "Screen turns off as usual" });
            }
            Hit::Close => self.set_mode(Mode::Read, p),
            Hit::Prev => self.step_match(-1),
            Hit::Next => self.step_match(1),
            Hit::Field => {
                let kb = match self.mode {
                    Mode::Goto => KB_NUMBER,
                    Mode::Password => KB_PASSWORD,
                    _ => KB_TEXT,
                };
                p.show_keyboard(kb, &self.field);
                self.keyboard = true;
            }
            Hit::Indicator => {
                if self.mode == Mode::Read {
                    self.set_mode(Mode::Goto, p);
                }
            }
            Hit::Bar => {}
        }
    }

    /// Switches between one page and two pages per row, keeping the current page.
    fn toggle_spread(&mut self) {
        self.spread = !self.spread;
        self.store.set_spread(self.spread);
        let pos = self.position().map(|p| Position { zoom: 1.0, x: 0.0, ..p });
        if let Some(d) = self.doc.as_mut() {
            if d.layout.is_some() {
                d.layout = None;
                d.restore = pos;
            }
        }
        self.clear_tiles = true;
        self.sent.clear();
        self.wanted.clear();
        self.ensure_layout();
        self.toast(if self.spread { "Two pages side by side" } else { "One page per row" });
    }

    fn open_panel(&mut self) {
        let Some(d) = &self.doc else { return };
        if d.outline.is_empty() {
            self.toast("This document has no chapters");
            return;
        }
        let cur = self.current_chapter().unwrap_or(0);
        self.center_chapter(cur);
        self.panel.open = true;
        self.panel.fling = None;
    }

    /// Scrolls the contents list so chapter `i` is in the middle.
    fn center_chapter(&mut self, i: usize) {
        let visible = (self.vh - self.insets[3] - self.panel_top()) as f64;
        self.panel.scroll = (i as f64 * self.row_h() - visible / 2.0 + self.row_h() / 2.0).clamp(0.0, self.panel_max_scroll());
    }

    /// Pins or unpins the contents panel; docks it right away if the window allows.
    fn toggle_pin(&mut self) {
        self.pin_chapters = !self.pin_chapters;
        self.store.set_pin_chapters(self.pin_chapters);
        if !self.pin_chapters {
            self.panel.open = false;
        }
        self.update_dock(self.vh);
        if self.pin_chapters && !self.docked() {
            self.toast("Contents will stay pinned when the window is wide enough");
        }
    }

    fn current_chapter(&self) -> Option<usize> {
        let d = self.doc.as_ref()?;
        let l = d.layout.as_ref()?;
        let y = self.view.y + self.vh as f64 / self.view.zoom * 0.15;
        let mut best = None;
        let mut best_y = f64::MIN;
        for (i, it) in d.outline.iter().enumerate() {
            let iy = l.y_of(it.page as usize, it.y.unwrap_or(0.0) as f64);
            if iy <= y + 1.0 && iy >= best_y {
                best = Some(i);
                best_y = iy;
            }
        }
        best
    }

    fn scrollable(&self) -> bool {
        self.layout().is_some_and(|l| l.total * self.view.zoom > self.vh as f64 * 1.5)
    }

    fn scrollbar_track(&self) -> (f32, f32) {
        let top = self.insets[1] + self.dp(8.0);
        let bottom = self.vh - self.insets[3] - self.dp(8.0);
        (top, bottom.max(top + 1.0))
    }

    fn scrollbar_to(&mut self, y: f32) {
        let Some(l) = self.layout() else { return };
        let (top, bottom) = self.scrollbar_track();
        let thumb = self.dp(44.0);
        let f = ((y - top - thumb / 2.0) / (bottom - top - thumb).max(1.0)).clamp(0.0, 1.0) as f64;
        let max = (l.total - self.vh as f64 / self.view.zoom).max(0.0);
        self.set_view(View { y: f * max, ..self.view });
        self.touch_activity();
    }

    // ----- per-frame update -------------------------------------------------------------

    pub fn update(&mut self, now: f64, p: &mut dyn Platform) {
        let dt = (now - self.last_update).clamp(0.0, 0.1) as f32;
        self.last_update = now;
        self.now = now;

        if let Some((at, x, y)) = self.tap_at {
            if now >= at {
                self.tap_at = None;
                self.single_tap(x, y, p);
            }
        }

        if let Some(mut f) = self.fling.take() {
            let ((dx, dy), running) = f.step(now);
            let z = self.view.zoom;
            let want = View { x: self.view.x - dx / z, y: self.view.y - dy / z, ..self.view };
            let got = self.clamp_view(want);
            if (got.x - want.x).abs() > 1e-6 {
                f.stop_axis(true);
            }
            if (got.y - want.y).abs() > 1e-6 {
                f.stop_axis(false);
            }
            if got != self.view {
                self.view = got;
                self.view_changed();
                self.touch_activity();
            }
            if running && !f.is_dead() {
                self.fling = Some(f);
            }
        }

        if let Some(a) = &self.anim {
            let u = ((now - a.start) / a.dur).clamp(0.0, 1.0);
            let e = ease_out(u);
            let (f, t) = (a.from, a.to);
            let zoom = (f.zoom.ln() + (t.zoom.ln() - f.zoom.ln()) * e).exp();
            let v = View { zoom, x: f.x + (t.x - f.x) * e, y: f.y + (t.y - f.y) * e };
            if u >= 1.0 {
                self.anim = None;
            }
            let v = if u >= 1.0 { self.clamp_view(t) } else { v };
            if v != self.view {
                self.view = v;
                self.view_changed();
            }
        }

        if let Some(mut f) = self.panel.fling.take() {
            let ((_, dy), running) = f.step(now);
            let max = self.panel_max_scroll();
            let s = self.panel.scroll - dy;
            self.panel.scroll = s.clamp(0.0, max);
            if running && s == self.panel.scroll {
                self.panel.fling = Some(f);
            }
            self.dirty = true;
        }

        if self.overlay && self.mode == Mode::Read && !self.panel.open && !self.menu && now >= self.overlay_until {
            self.overlay = false;
        }
        let step = dt / 0.15;
        let target = self.overlay_target();
        self.dirty |= approach(&mut self.overlay_t, target, step);
        let target = self.indicator_target();
        self.dirty |= approach(&mut self.indicator_t, target, dt / 0.3);
        let target = if self.panel.open { 1.0 } else { 0.0 };
        self.dirty |= approach(&mut self.panel.t, target, dt / 0.18);

        if self.toast.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.toast = None;
            self.dirty = true;
        }
        if self.search.debounce.is_some_and(|d| now >= d) {
            self.start_search();
        }

        if self.docked() && self.panel.fling.is_none() && !matches!(self.gesture, Gesture::Panel { .. }) {
            let cur = self.current_chapter();
            if cur != self.panel.follow {
                self.panel.follow = cur;
                if let Some(c) = cur {
                    // Keep the current chapter in view as reading moves on.
                    let row = c as f64 * self.row_h();
                    let visible = (self.vh - self.insets[3] - self.panel_top()) as f64;
                    if row < self.panel.scroll || row + self.row_h() > self.panel.scroll + visible {
                        self.center_chapter(c);
                    }
                }
                self.dirty = true;
            }
        }

        self.shelf_update(now, p);

        if self.caffeine_applied != Some(self.caffeine) {
            self.caffeine_applied = Some(self.caffeine);
            p.keep_screen_on(self.caffeine);
        }
        let bars = self.overlay_visible();
        if self.bars != Some(bars) {
            self.bars = Some(bars);
            p.system_bars(bars);
        }

        let settled = self.anim.is_none() && !matches!(self.gesture, Gesture::Pinch { .. });
        if settled {
            let level = layout::level_for_zoom(self.view.zoom);
            if level != self.render_level {
                self.render_level = level;
                self.requests_dirty = true;
            }
        }
        if self.requests_dirty {
            self.update_requests();
        }
    }

    /// Screen rect of page `p`, snapped to whole pixels when rendering 1:1.
    fn page_rect(&self, l: &Layout, p: usize) -> [f64; 4] {
        let z = self.view.zoom;
        let (mut x0, mut y0) = self.view.to_screen(l.xs[p], l.tops[p]);
        let (mut x1, mut y1) = self.view.to_screen(l.xs[p] + l.widths[p], l.tops[p] + l.heights[p]);
        // Snap both edges so neighbouring pages share exactly the same pixel row.
        if (z - layout::level_scale(self.render_level)).abs() < 1e-9 {
            (x0, y0, x1, y1) = (x0.round(), y0.round(), x1.round(), y1.round());
        }
        [x0, y0, x1 - x0, y1 - y0]
    }

    /// Tiles of page `p` at `level` intersecting screen rect `r`, with their
    /// distance from the screen centre and GPU size in bytes.
    fn tiles_in(&self, l: &Layout, p: usize, level: i8, r: [f64; 4], urgent: bool, out: &mut Vec<(f64, RenderReq, usize)>) {
        let pr = self.page_rect(l, p);
        let g = layout::grid(l, p, level);
        let u0 = (r[0] - pr[0]) / pr[2];
        let u1 = (r[0] + r[2] - pr[0]) / pr[2];
        let v0 = (r[1] - pr[1]) / pr[3];
        let v1 = (r[1] + r[3] - pr[1]) / pr[3];
        let Some((cols, rows)) = g.cover(u0, v0, u1, v1) else { return };
        let (cx, cy) = (self.vw as f64 / 2.0, self.vh as f64 / 2.0);
        let bytes = g.tile_w as usize * g.tile_h as usize * 4;
        for row in rows {
            for col in cols.clone() {
                let (x, y, w, h) = g.tile_rect(col, row);
                let sx = pr[0] + (x as f64 + w as f64 / 2.0) / g.page_w as f64 * pr[2];
                let sy = pr[1] + (y as f64 + h as f64 / 2.0) / g.page_h as f64 * pr[3];
                let key = TileKey { page: p as u32, level, row: row as u16, col: col as u16 };
                let req = RenderReq { key, page_w: g.page_w, page_h: g.page_h, x, y, w, h, urgent };
                out.push(((sx - cx).hypot(sy - cy), req, bytes));
            }
        }
    }

    fn thumb_req(l: &Layout, p: usize, urgent: bool) -> RenderReq {
        let (w, h) = layout::thumb_dims(l, p);
        RenderReq { key: TileKey::thumb(p as u32), page_w: w, page_h: h, x: 0, y: 0, w, h, urgent }
    }

    /// Plans what should be on the GPU, in priority order, within the memory
    /// budget: visible tiles always; prefetch and nearby thumbnails only while
    /// they fit. Sends the missing ones to the worker (replacing its queue).
    fn update_requests(&mut self) {
        self.requests_dirty = false;
        let Some(d) = &self.doc else { return };
        let id = d.id;
        if self.shelf_visible() {
            // Nothing of the document is on screen.
            self.wanted.clear();
            if !self.sent.is_empty() {
                self.worker.set_queue(id, Vec::new());
                self.sent.clear();
            }
            return;
        }
        let Some(l) = &d.layout else { return };
        let (vw, vh) = (self.vw as f64, self.vh as f64);
        let v = self.view;
        let level = self.render_level;
        let screen = [0.0, 0.0, vw, vh];
        let doc_h = vh / v.zoom;
        let visible = l.range(v.y, v.y + doc_h);
        let cap = self.tiles.budget / 10 * 9;

        let mut wanted = HashSet::new();
        let mut order: Vec<RenderReq> = Vec::new();
        let mut bytes = 0usize;
        // Adds a request to the plan; optional ones only if they fit the budget.
        let mut plan = |r: RenderReq, b: usize, must: bool| -> bool {
            if wanted.contains(&r.key) {
                return true;
            }
            if !must && bytes + b > cap {
                return false;
            }
            bytes += b;
            wanted.insert(r.key);
            order.push(r);
            true
        };
        let thumb_bytes = |p: usize| {
            let (w, h) = layout::thumb_dims(l, p);
            w as usize * h as usize * 4
        };

        let mut vis = Vec::new();
        for p in visible.clone() {
            let start = vis.len();
            self.tiles_in(l, p, level, screen, true, &mut vis);
            let complete = vis[start..].iter().all(|(_, r, _)| self.tiles.contains(&r.key));
            // A thumbnail covers the page until its tiles arrive.
            if !complete || self.tiles.contains(&TileKey::thumb(p as u32)) {
                plan(Self::thumb_req(l, p, true), thumb_bytes(p), true);
            }
        }
        vis.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, r, b) in vis {
            plan(r, b, true);
        }

        // Prefetch: a screen ahead in the scroll direction, half a screen behind.
        let ahead = if self.scroll_dir >= 0.0 { [0.0, vh, vw, vh] } else { [0.0, -vh, vw, vh] };
        let behind = if self.scroll_dir >= 0.0 { [0.0, -vh * 0.5, vw, vh * 0.5] } else { [0.0, vh, vw, vh * 0.5] };
        'prefetch: for r in [ahead, behind] {
            let mut pre = Vec::new();
            let (dy0, dy1) = (v.y + r[1] / v.zoom, v.y + (r[1] + r[3]) / v.zoom);
            for p in l.range(dy0, dy1) {
                self.tiles_in(l, p, level, r, false, &mut pre);
            }
            pre.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (_, r, b) in pre {
                if !plan(r, b, false) {
                    break 'prefetch;
                }
            }
        }

        // Thumbnails around the viewport: cheap placeholders for fast scrolling.
        let near = l.range(v.y - 2.0 * doc_h, v.y + 3.0 * doc_h);
        let mut thumbs: Vec<usize> = near.filter(|p| !visible.contains(p)).collect();
        let cur = visible.start as f64;
        thumbs.sort_by(|a, b| (*a as f64 - cur).abs().total_cmp(&(*b as f64 - cur).abs()));
        for p in thumbs.into_iter().take(12) {
            if !plan(Self::thumb_req(l, p, false), thumb_bytes(p), false) {
                break;
            }
        }

        let reqs: Vec<RenderReq> =
            order.into_iter().filter(|r| !self.tiles.contains(&r.key) && !self.in_flight.contains(&r.key)).take(160).collect();
        self.wanted = wanted;
        if reqs != self.sent {
            self.worker.set_queue(id, reqs.clone());
            self.sent = reqs;
        }
    }

    // ----- rendering --------------------------------------------------------------------

    fn process_uploads(&mut self, gpu: &mut dyn Gpu) {
        let mut count = 0;
        let mut bytes = 0;
        while let Some(u) = self.uploads.pop_front() {
            self.in_flight.remove(&u.key);
            let dims = match &self.doc {
                Some(d) if d.id == u.doc && self.wanted.contains(&u.key) => d.layout.as_ref().and_then(|l| {
                    let p = u.key.page as usize;
                    if p >= l.len() {
                        None
                    } else if u.key.is_thumb() {
                        Some((u.w, u.h))
                    } else {
                        let g = layout::grid(l, p, u.key.level);
                        (u.w <= g.tile_w && u.h <= g.tile_h).then_some((g.tile_w, g.tile_h))
                    }
                }),
                _ => None,
            };
            if let Some((tw, th)) = dims {
                self.tiles.insert(gpu, u.key, u.w, u.h, tw, th, &u.pixels, self.frame);
                count += 1;
                bytes += u.pixels.len();
            }
            self.worker.recycle(u.pixels);
            if count >= 4 || bytes >= 8 << 20 {
                break;
            }
        }
    }

    pub fn render(&mut self, gpu: &mut dyn Gpu) {
        self.frame += 1;
        self.dirty = false;
        if self.text.begin_frame() {
            self.dirty = true;
        }
        if self.clear_tiles {
            self.clear_tiles = false;
            self.tiles.clear(gpu);
            for u in std::mem::take(&mut self.uploads) {
                self.worker.recycle(u.pixels);
            }
            self.in_flight.clear();
            self.requests_dirty = true;
        }
        self.process_uploads(gpu);
        if !self.uploads.is_empty() {
            self.dirty = true;
        }
        if self.shelf_visible() {
            return self.render_shelf(gpu);
        }
        self.covers.clear_gpu(gpu);

        gpu.begin(self.win_w as u32, self.vh as u32, ui::BG);
        let atlas = self.text.flush(gpu);
        let ox = self.ox;
        let mut under = Painter { ox, ..Default::default() };
        let mut draws: Vec<(TileKey, [f32; 4])> = Vec::new();
        if let Some(l) = self.layout() {
            let v = self.view;
            let level = self.render_level;
            let screen = [0.0, 0.0, self.vw as f64, self.vh as f64];
            for p in l.range(v.y, v.y + self.vh as f64 / v.zoom) {
                let pr = self.page_rect(l, p);
                under.rect(pr[0] as f32, pr[1] as f32, pr[2] as f32, pr[3] as f32, ui::PLACEHOLDER, 0.0);
                let mut cur = Vec::new();
                self.tiles_in(l, p, level, screen, true, &mut cur);
                let complete = cur.iter().all(|(_, r, _)| self.tiles.contains(&r.key));
                if !complete {
                    let mut others: Vec<TileKey> = self.tiles.page(p as u32).map(|(k, _)| *k).filter(|k| k.level != level).collect();
                    others.sort_by_key(|k| if k.is_thumb() { i32::MIN } else { -(k.level as i32 - level as i32).abs() });
                    for k in others {
                        let r = self.tile_rect(l, &k, pr);
                        if r[0] < screen[2] && r[1] < screen[3] && r[0] + r[2] > 0.0 && r[1] + r[3] > 0.0 {
                            draws.push((k, r.map(|c| c as f32)));
                        }
                    }
                }
                for (_, r, _) in cur {
                    if self.tiles.contains(&r.key) {
                        draws.push((r.key, self.tile_rect(l, &r.key, pr).map(|c| c as f32)));
                    }
                }
            }
        }
        gpu.draw_ui(&under.v, atlas);
        for (k, r) in &draws {
            self.tiles.touch(k, self.frame);
            if let Some(t) = self.tiles.get(k) {
                gpu.draw_texture(t.tex, [r[0] + ox, r[1], r[2], r[3]], t.uv(), t.clamp());
            }
        }

        let mut over = Painter { ox, ..Default::default() };
        self.draw_highlights(&mut over);
        self.draw_overlay(&mut over);
        let atlas = self.text.flush(gpu);
        gpu.draw_ui(&over.v, atlas);

        self.tiles.evict(gpu, self.frame, &self.wanted);
    }

    fn tile_rect(&self, l: &Layout, k: &TileKey, pr: [f64; 4]) -> [f64; 4] {
        if k.is_thumb() {
            return pr;
        }
        let g = layout::grid(l, k.page as usize, k.level);
        let (x, y, w, h) = g.tile_rect(k.col as u32, k.row as u32);
        let (sx, sy) = (pr[2] / g.page_w as f64, pr[3] / g.page_h as f64);
        [pr[0] + x as f64 * sx, pr[1] + y as f64 * sy, w as f64 * sx, h as f64 * sy]
    }

    fn draw_highlights(&self, p: &mut Painter) {
        if self.search.hits.is_empty() {
            return;
        }
        let Some(l) = self.layout() else { return };
        let v = self.view;
        let range = l.range(v.y, v.y + self.vh as f64 / v.zoom);
        let start = self.search.hits.partition_point(|m| (m.page as usize) < range.start);
        let pad = self.dp(1.5);
        for (i, m) in self.search.hits.iter().enumerate().skip(start) {
            if m.page as usize >= range.end {
                break;
            }
            let pr = self.page_rect(l, m.page as usize);
            let color = if Some(i) == self.search.current { ui::HIT_CURRENT } else { ui::HIT };
            for r in &m.rects {
                let x = pr[0] + r[0] as f64 * pr[2];
                let y = pr[1] + r[1] as f64 * pr[3];
                let w = (r[2] - r[0]) as f64 * pr[2];
                let h = (r[3] - r[1]) as f64 * pr[3];
                p.rect(x as f32 - pad, y as f32 - pad, w as f32 + 2.0 * pad, h as f32 + 2.0 * pad, color, pad * 1.5);
            }
        }
    }

    fn icon_button(&mut self, p: &mut Painter, icon: Icon, x: f32, cy: f32, hit: Hit, a: f32) {
        let b = self.dp(48.0);
        let r = [x, cy - b / 2.0, b, b];
        if matches!(self.gesture, Gesture::Button { hit: h, inside: true, .. } if h == hit) {
            p.circle(x + b / 2.0, cy, b / 2.0 - self.dp(2.0), fade(ui::PRESS, a));
        }
        let on = match hit {
            Hit::Chapters => self.docked(),
            Hit::Spread => self.spread,
            Hit::Caffeine => self.caffeine,
            Hit::Settings => self.menu,
            _ => false,
        };
        let color = if on { ui::ACCENT } else { ui::FG };
        ui::icon(p, icon, x + b / 2.0, cy, self.dp(24.0), fade(color, a));
        self.buttons.push(Button { r, hit });
    }

    fn draw_field(&mut self, p: &mut Painter, x: f32, w: f32, cy: f32, placeholder: &str) {
        let h = self.dp(38.0);
        p.rect(x, cy - h / 2.0, w, h, ui::FIELD, h / 2.0);
        self.buttons.push(Button { r: [x, cy - h / 2.0, w, h], hit: Hit::Field });
        let px = self.dp(16.0);
        let pad = self.dp(14.0);
        let inner = w - 2.0 * pad;
        let baseline = cy + self.text.cap_height(px) / 2.0;
        let shown: String = if self.mode == Mode::Password { "•".repeat(self.field.chars().count()) } else { self.field.clone() };
        if shown.is_empty() {
            self.text.draw(p, placeholder, x + pad, baseline, px, ui::DIM, inner);
            if self.keyboard {
                p.rect(x + pad, cy - self.dp(10.0), self.dp(2.0), self.dp(20.0), ui::ACCENT, 0.0);
            }
            return;
        }
        // Keep the end of long text visible.
        let chars: Vec<char> = shown.chars().collect();
        let mut start = 0;
        while start < chars.len() && self.text.measure(&chars[start..].iter().collect::<String>(), px) > inner {
            start += 1;
        }
        let visible: String = chars[start..].iter().collect();
        self.text.draw(p, &visible, x + pad, baseline, px, ui::FG, inner + 1.0);
        if self.keyboard {
            let cur = self.cursor.clamp(start, chars.len());
            let before: String = chars[start..cur].iter().collect();
            let cx = x + pad + self.text.measure(&before, px);
            p.rect(cx, cy - self.dp(10.0), self.dp(2.0), self.dp(20.0), ui::ACCENT, 0.0);
        }
    }

    fn draw_overlay(&mut self, p: &mut Painter) {
        self.buttons.clear();
        let a = self.overlay_t;
        let (vw, vh) = (self.vw, self.vh);
        let b = self.dp(48.0);
        if a > 0.01 {
            let top = self.insets[1];
            p.rect(0.0, 0.0, vw, self.bar_bottom(), fade(ui::BAR, a), 0.0);
            if a > 0.5 {
                self.buttons.push(Button { r: [0.0, 0.0, vw, self.bar_bottom()], hit: Hit::Bar });
            }
            let cy = top + self.bar_h() / 2.0;
            let left = (self.insets[0] - self.ox).max(0.0) + self.dp(4.0);
            let right = vw - self.insets[2] - self.dp(4.0);
            match self.mode {
                Mode::Read => {
                    let open = if self.home { Icon::Library } else { Icon::Open };
                    self.icon_button(p, open, left, cy, Hit::Open, a);
                    let mut rx = right - b;
                    self.icon_button(p, Icon::Settings, rx, cy, Hit::Settings, a);
                    let has_outline = self.doc.as_ref().is_some_and(|d| !d.outline.is_empty());
                    if self.doc.is_some() {
                        if has_outline {
                            rx -= b;
                            self.icon_button(p, Icon::Chapters, rx, cy, Hit::Chapters, a);
                        }
                        rx -= b;
                        self.icon_button(p, Icon::Search, rx, cy, Hit::Search, a);
                        rx -= b;
                        self.icon_button(p, Icon::Spread, rx, cy, Hit::Spread, a);
                        rx -= b;
                        self.icon_button(p, Icon::Caffeine, rx, cy, Hit::Caffeine, a);
                    }
                    let title = match (&self.doc, &self.pending) {
                        (Some(d), _) => d.name.clone(),
                        (None, Some(pd)) => pd.name.clone(),
                        _ => String::from("JustPDF"),
                    };
                    let px = self.dp(17.0);
                    let tx = left + b + self.dp(8.0);
                    let baseline = cy + self.text.cap_height(px) / 2.0;
                    self.text.draw(p, &title, tx, baseline, px, fade(ui::FG, a), (rx - self.dp(8.0) - tx).max(0.0));
                }
                Mode::Search => {
                    self.icon_button(p, Icon::Back, left, cy, Hit::Close, a);
                    let nx = right - b;
                    self.icon_button(p, Icon::Down, nx, cy, Hit::Next, a);
                    let px_ = nx - b;
                    self.icon_button(p, Icon::Up, px_, cy, Hit::Prev, a);
                    let n = self.search.hits.len();
                    let count = match (self.search.current, n) {
                        (Some(c), n) if n > 0 => format!("{}/{}", c + 1, n),
                        _ if self.search.running => String::from("…"),
                        _ if !self.search.query.is_empty() => String::from("0"),
                        _ => String::new(),
                    };
                    let cpx = self.dp(14.0);
                    let cw = self.text.measure(&count, cpx);
                    let cx = px_ - self.dp(4.0) - cw;
                    let baseline = cy + self.text.cap_height(cpx) / 2.0;
                    self.text.draw(p, &count, cx, baseline, cpx, fade(ui::DIM, a), cw + 1.0);
                    let fx = left + b + self.dp(4.0);
                    let fw = (cx - self.dp(10.0) - fx).max(self.dp(40.0));
                    self.draw_field(p, fx, fw, cy, "Search");
                    if self.search.running && self.search.total > 0 {
                        let f = self.search.scanned as f32 / self.search.total as f32;
                        p.rect(0.0, self.bar_bottom() - self.dp(2.0), vw * f, self.dp(2.0), ui::ACCENT, 0.0);
                    }
                }
                Mode::Goto | Mode::Password => {
                    self.icon_button(p, Icon::Back, left, cy, Hit::Close, a);
                    let fx = left + b + self.dp(4.0);
                    let fw = right - fx - self.dp(8.0);
                    let hint = if self.mode == Mode::Goto {
                        format!("Go to page (1–{})", self.doc.as_ref().map_or(0, |d| d.sizes.len()))
                    } else {
                        String::from("Password")
                    };
                    self.draw_field(p, fx, fw, cy, &hint);
                }
            }
        }

        // Page indicator and scrollbar.
        let sa = a.max(self.indicator_t);
        if sa > 0.01 && self.mode != Mode::Password {
            if let Some((n, total, row)) = self.layout().map(|l| (l.len(), l.total, l.row_pages(self.current_page()))) {
                let label =
                    if row.len() > 1 { format!("{}–{} / {}", row.start + 1, row.end, n) } else { format!("{} / {}", row.start + 1, n) };
                let px = self.dp(14.0);
                let w = self.text.measure(&label, px) + self.dp(28.0);
                let h = self.dp(32.0);
                let x = (vw - w) / 2.0;
                let y = vh - self.insets[3] - self.dp(20.0) - h;
                p.rect(x, y, w, h, fade(ui::BAR, sa), h / 2.0);
                let baseline = y + h / 2.0 + self.text.cap_height(px) / 2.0;
                self.text.draw(p, &label, x + self.dp(14.0), baseline, px, fade(ui::FG, sa), w);
                if a > 0.5 {
                    self.buttons.push(Button { r: [x, y - self.dp(8.0), w, h + self.dp(16.0)], hit: Hit::Indicator });
                }
                if self.scrollable() {
                    let (top, bottom) = self.scrollbar_track();
                    let th = self.dp(44.0);
                    let max = (total - vh as f64 / self.view.zoom).max(1.0);
                    let f = (self.view.y / max).clamp(0.0, 1.0) as f32;
                    let ty = top + f * (bottom - top - th);
                    let tw = if matches!(self.gesture, Gesture::Scrollbar) { self.dp(8.0) } else { self.dp(5.0) };
                    let tx = vw - self.insets[2] - self.dp(4.0) - tw;
                    p.rect(tx, ty, tw, th, fade(ui::THUMB, sa), tw / 2.0);
                }
            }
        }

        if self.docked() || self.panel.t > 0.001 {
            // The panel lives in window coordinates, left of the document viewport.
            let ox = std::mem::replace(&mut p.ox, 0.0);
            self.draw_panel(p);
            p.ox = ox;
        }

        if self.doc.is_none() {
            if let Some(pd) = &self.pending {
                if self.now - pd.since > 0.3 {
                    let msg = format!("Opening {}…", pd.name);
                    let px = self.dp(15.0);
                    let w = self.text.measure(&msg, px).min(vw - self.dp(32.0));
                    self.text.draw(p, &msg, (vw - w) / 2.0, vh / 2.0, px, ui::DIM, w + 1.0);
                }
            }
        }

        if self.menu {
            // The menu lives in window coordinates (right edge of the window).
            let ox = std::mem::replace(&mut p.ox, 0.0);
            self.draw_menu(p);
            p.ox = ox;
        }
        self.draw_toast(p, 0.0, vw);
    }

    /// The settings menu card, under the top bar at the right (window coordinates).
    fn menu_rect(&self) -> [f32; 4] {
        let w = self.dp(320.0).min(self.win_w - self.dp(16.0));
        let x = self.win_w - self.insets[2] - self.dp(8.0) - w;
        [x.max(self.dp(8.0)), self.bar_bottom() + self.dp(4.0), w, self.dp(72.0)]
    }

    fn draw_menu(&mut self, p: &mut Painter) {
        let r = self.menu_rect();
        p.rect(r[0] - 1.0, r[1] - 1.0, r[2] + 2.0, r[3] + 2.0, [0, 0, 0, 90], self.dp(13.0));
        p.rect(r[0], r[1], r[2], r[3], [44, 44, 50, 255], self.dp(12.0));
        if matches!(self.gesture, Gesture::Button { hit: Hit::MenuHome, inside: true, .. }) {
            p.rect(r[0], r[1], r[2], r[3], ui::PRESS, self.dp(12.0));
        }
        self.buttons.push(Button { r, hit: Hit::MenuHome });
        // Switch at the right.
        let (sw, sh) = (self.dp(40.0), self.dp(22.0));
        let sx = r[0] + r[2] - self.dp(16.0) - sw;
        let cy = r[1] + r[3] / 2.0;
        p.rect(sx, cy - sh / 2.0, sw, sh, if self.home { ui::ACCENT } else { [90, 90, 98, 255] }, sh / 2.0);
        let kx = if self.home { sx + sw - sh / 2.0 } else { sx + sh / 2.0 };
        p.circle(kx, cy, sh / 2.0 - self.dp(3.0), if self.home { [16, 24, 40, 255] } else { ui::FG });
        let (tpx, spx) = (self.dp(15.0), self.dp(12.5));
        let tx = r[0] + self.dp(16.0);
        let max = (sx - self.dp(12.0) - tx).max(0.0);
        self.text.draw(p, "Library at start", tx, cy - self.dp(4.0), tpx, ui::FG, max);
        let sub = if self.home { "Book covers when the app opens" } else { "Off: the file picker opens, as before" };
        self.text.draw(p, sub, tx, cy + self.dp(16.0), spx, ui::DIM, max);
    }

    /// Touches while the settings menu is open: its row toggles, anything else closes it.
    fn menu_touch(&mut self, action: i32, index: usize, p: &mut dyn Platform) {
        let (x, y) = self.pointers.get(index).map_or((0.0, 0.0), |&(_, x, y)| (x, y));
        let r = self.menu_rect();
        match action {
            ACTION_DOWN => {
                self.gesture = if inside(r, x, y) {
                    Gesture::Button { hit: Hit::MenuHome, r, inside: true }
                } else {
                    // Closing the menu takes the whole gesture.
                    self.menu = false;
                    Gesture::None
                };
            }
            ACTION_MOVE => {
                if let (Gesture::Button { hit, r, .. }, Some(&(_, cx, cy))) = (self.gesture, self.pointers.first()) {
                    self.gesture = Gesture::Button { hit, r, inside: inside(r, cx, cy) };
                }
            }
            ACTION_UP => {
                if let Gesture::Button { hit, r, .. } = std::mem::replace(&mut self.gesture, Gesture::None) {
                    if inside(r, x, y) {
                        self.press(hit, p);
                    }
                }
            }
            _ => self.gesture = Gesture::None,
        }
    }

    /// The toast message, centred in the `w` wide area starting at `x0`.
    fn draw_toast(&mut self, p: &mut Painter, x0: f32, w: f32) {
        let Some((msg, _)) = &self.toast else { return };
        let msg = msg.clone();
        let px = self.dp(14.0);
        let tw = (self.text.measure(&msg, px) + self.dp(32.0)).min(w - self.dp(32.0));
        let h = self.dp(40.0);
        let x = x0 + (w - tw) / 2.0;
        let y = self.vh - self.insets[3] - self.dp(72.0) - h;
        p.rect(x, y, tw, h, [48, 48, 54, 245], h / 2.0);
        let baseline = y + h / 2.0 + self.text.cap_height(px) / 2.0;
        self.text.draw(p, &msg, x + self.dp(16.0), baseline, px, ui::FG, tw - self.dp(32.0));
    }

    fn draw_panel(&mut self, p: &mut Painter) {
        let docked = self.docked();
        let t = if docked { 1.0 } else { ease_out(self.panel.t as f64) as f32 };
        let vh = self.vh;
        let pw = self.panel_w();
        let x0 = -pw * (1.0 - t);
        if !docked {
            p.rect(0.0, 0.0, self.win_w, vh, [0, 0, 0, (130.0 * t) as u8], 0.0);
        }
        p.rect(x0, 0.0, pw, vh, ui::PANEL, 0.0);
        let resizing = matches!(self.gesture, Gesture::PanelResize { .. });
        if docked {
            // Shrinking preview: blank the strip the document will move into.
            p.rect(pw, 0.0, self.ox - pw, vh, [18, 18, 20, 255], 0.0);
            p.rect(pw - 1.0, 0.0, 1.0, vh, [255, 255, 255, 18], 0.0);
        }
        let Some(d) = &self.doc else { return };
        let items = d.outline.clone();
        let n_pages = d.sizes.len();
        let current = self.current_chapter();
        let top = self.panel_top();
        let row_h = self.row_h();
        let bottom = vh - self.insets[3];
        let first = (self.panel.scroll / row_h).floor().max(0.0) as usize;
        // Keep text clear of a left display cutout / navigation bar.
        let il = self.insets[0];
        let tpx = self.dp(15.0);
        let npx = self.dp(13.0);
        for (i, it) in items.iter().enumerate().skip(first) {
            let y = top + (i as f64 * row_h - self.panel.scroll) as f32;
            if y > bottom {
                break;
            }
            let rh = row_h as f32;
            if Some(i) == current {
                p.rect(x0, y, pw, rh, ui::ROW_SEL, 0.0);
                p.rect(x0, y + self.dp(8.0), self.dp(3.0), rh - self.dp(16.0), ui::ACCENT, self.dp(1.5));
            }
            let num = format!("{}", (it.page as usize + 1).min(n_pages));
            let nw = self.text.measure(&num, npx);
            let nx = x0 + pw - self.dp(16.0) - nw;
            let base = y + rh / 2.0;
            self.text.draw(p, &num, nx, base + self.text.cap_height(npx) / 2.0, npx, ui::DIM, nw + 1.0);
            let indent = il + self.dp(20.0) + self.dp(14.0) * it.depth.min(6) as f32;
            let tx = x0 + indent;
            let color = if it.depth == 0 { ui::FG } else { [205, 205, 212, 255] };
            let title = if it.title.is_empty() { "—" } else { it.title.as_str() };
            self.text.draw(p, title, tx, base + self.text.cap_height(tpx) / 2.0, tpx, color, (nx - self.dp(12.0) - tx).max(0.0));
        }
        // Header drawn last so rows scroll underneath it.
        p.rect(x0, 0.0, pw, top, ui::PANEL, 0.0);
        let hpx = self.dp(18.0);
        let baseline = self.insets[1] + self.dp(28.0) + self.text.cap_height(hpx) / 2.0;
        let pin = self.pin_rect();
        self.text.draw(p, "Contents", x0 + il + self.dp(20.0), baseline, hpx, ui::FG, (pin[0] - il - self.dp(24.0)).max(0.0));
        let (bx, cy) = (x0 + pin[0] + pin[2] / 2.0, pin[1] + pin[3] / 2.0);
        if matches!(self.gesture, Gesture::Panel { x, start, moved: false, .. } if inside(pin, x, start)) {
            p.circle(bx, cy, pin[2] / 2.0 - self.dp(2.0), ui::PRESS);
        }
        let color = if self.pin_chapters { ui::ACCENT } else { ui::DIM };
        ui::icon(p, Icon::Pin, bx, cy, self.dp(22.0), color);
        p.rect(x0, top - 1.0, pw, 1.0, [255, 255, 255, 18], 0.0);
        if docked {
            // Resize handle on the edge.
            let (hw, hh) = if resizing { (self.dp(5.0), self.dp(56.0)) } else { (self.dp(4.0), self.dp(40.0)) };
            let hy = top + (bottom - top) / 2.0;
            let color = if resizing { ui::ACCENT } else { fade(ui::DIM, 0.7) };
            p.rect(pw - hw / 2.0, hy - hh / 2.0, hw, hh, color, hw / 2.0);
        }
    }
}

fn utf16_to_char_index(s: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, c) in s.chars().enumerate() {
        if units >= utf16 {
            return i;
        }
        units += c.len_utf16();
    }
    s.chars().count()
}
