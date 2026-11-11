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
    Split,
    Caffeine,
    /// Back to the default size: zoom 1, pages fill the width.
    FitWidth,
    Bar,
    ShelfBack,
    ShelfOpenFile,
    ShelfAddFolder,
    ShelfRemove,
    Settings,
    /// The "library at start" row of the settings menu.
    MenuHome,
    /// The "two pages side by side" row of the settings menu.
    MenuSpread,
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
    /// Touch in the contents panel; `x`, `start`: pane coordinates where it began.
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

/// One reader: a document (or the library) with its own view, tiles, PDF
/// worker and overlay. Normally there is one; split view shows two side by
/// side (top and bottom in a portrait window).
struct Pane {
    /// Never reused; answers from the platform are routed back by it.
    id: u64,
    /// 0: left / top, 1: right / bottom (split view only).
    slot: u8,
    /// The pane's rect in the window: origin `x0`, `y0`, width `w`, height `vh`.
    /// The document viewport is `vw` wide starting at pane x `ox` (non-zero
    /// while the contents panel is docked on the left).
    x0: f32,
    y0: f32,
    w: f32,
    ox: f32,
    vw: f32,
    vh: f32,
    /// Safe-area insets of the window edges this pane touches.
    insets: [f32; 4],
    shelf: Shelf,
    worker: Worker,
    doc: Option<Doc>,
    pending: Option<Pending>,

    view: View,
    anim: Option<Anim>,
    fling: Option<Fling>,
    vel: VelocityTracker,
    scroll_dir: f64,
    gesture: Gesture,
    last_tap: Option<(f64, f32, f32)>,
    tap_at: Option<(f64, f32, f32)>,

    mode: Mode,
    field: String,
    cursor: usize,
    overlay: bool,
    overlay_t: f32,
    overlay_until: f64,
    activity: f64,
    indicator_t: f32,
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
    buttons: Vec<Button>,
}

impl Pane {
    fn new(id: u64, slot: u8, sink: Sink, shelf: Shelf) -> Pane {
        Pane {
            id,
            slot,
            x0: 0.0,
            y0: 0.0,
            w: 0.0,
            ox: 0.0,
            vw: 0.0,
            vh: 0.0,
            insets: [0.0; 4],
            shelf,
            worker: Worker::new(sink),
            doc: None,
            pending: None,
            view: View::default(),
            anim: None,
            fling: None,
            vel: VelocityTracker::default(),
            scroll_dir: 1.0,
            gesture: Gesture::None,
            last_tap: None,
            tap_at: None,
            mode: Mode::Read,
            field: String::new(),
            cursor: 0,
            overlay: false,
            overlay_t: 0.0,
            overlay_until: 0.0,
            activity: -10.0,
            indicator_t: 0.0,
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
            buttons: Vec::new(),
        }
    }

    /// The document (being) opened here has worker id `doc`.
    fn owns(&self, doc: u64) -> bool {
        self.doc.as_ref().is_some_and(|d| d.id == doc) || self.pending.as_ref().is_some_and(|p| p.id == doc)
    }

    fn contains(&self, x: f32, y: f32) -> bool {
        inside([self.x0, self.y0, self.w, self.vh], x, y)
    }
}

pub struct App {
    density: f32,
    /// Window size and safe-area insets (left, top, right, bottom).
    win_w: f32,
    win_h: f32,
    win_insets: [f32; 4],
    now: f64,
    last_update: f64,
    store: Store,
    library: Library,
    covers: Covers,
    sink: Sink,
    next_id: u64,
    /// Library folders whose scan is running, and when they were last scanned.
    scanning: HashSet<String>,
    last_scan: Option<f64>,

    /// The pane with the focus (gets keys, text and touches that start in it).
    pane: Pane,
    /// The other half of the split view.
    other: Option<Box<Pane>>,
    /// `other`'s `pane_animating()` / `pane_deadline()`, taken when it lost the focus
    /// (it only changes while it is swapped in, see `swap_panes`).
    other_busy: (bool, Option<f64>),
    /// Panes closed since the last frame: their tiles go with the next render.
    retired: Vec<Pane>,
    /// The pane waiting for an answer from the file picker / `open_document`.
    opener: Option<u64>,

    pointers: Vec<(i32, f32, f32)>,
    keyboard: bool,
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
    text: Text,
    frame: u64,
    dirty: bool,
}

const TAP_TIMEOUT: f64 = 0.25;
const DOUBLE_TAP_GAP: f64 = 0.3;
const OVERLAY_TIMEOUT: f64 = 4.0;
/// The settings menu, top to bottom.
const MENU_ROWS: [Hit; 2] = [Hit::MenuHome, Hit::MenuSpread];

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
            win_h: 0.0,
            win_insets: [0.0; 4],
            now: 0.0,
            last_update: 0.0,
            store,
            library,
            covers,
            pane: Pane::new(1, 0, sink.clone(), shelf),
            sink,
            next_id: 2,
            scanning: HashSet::new(),
            last_scan: None,
            other: None,
            other_busy: (false, None),
            retired: Vec::new(),
            opener: None,
            pointers: Vec::new(),
            keyboard: false,
            bars: None,
            spread,
            caffeine,
            caffeine_applied: None,
            pin_chapters,
            chapters_w,
            home,
            text: Text::new(),
            frame: 0,
            dirty: true,
        }
    }

    fn dp(&self, v: f32) -> f32 {
        v * self.density
    }

    // ----- queries used by the platform loop -------------------------------------------

    pub fn has_document(&self) -> bool {
        self.pane.doc.is_some() || self.other.as_ref().is_some_and(|o| o.doc.is_some())
    }

    /// The document to reopen after a restart: the left / top one.
    pub fn current_uri(&self) -> Option<&str> {
        let first = match &self.other {
            Some(o) if o.slot < self.pane.slot => o,
            _ => &self.pane,
        };
        first.doc.as_ref().or(self.pane.doc.as_ref()).map(|d| d.uri.as_str())
    }

    /// Cache statistics for diagnostics (the focused pane).
    pub fn stats(&self) -> String {
        format!(
            "tiles={} bytes={}MB uploads={} queued={} wanted={} level={}{}",
            self.pane.tiles.len(),
            self.pane.tiles.bytes() >> 20,
            self.pane.uploads.len(),
            self.pane.sent.len(),
            self.pane.wanted.len(),
            self.pane.render_level,
            if self.split() { format!(" split(focus={})", self.pane.slot) } else { String::new() }
        )
    }

    pub fn mode(&self) -> Mode {
        self.pane.mode
    }

    pub fn view(&self) -> View {
        self.pane.view
    }

    /// Whether a back press would be handled by the app (rather than exiting).
    pub fn back_consumable(&self) -> bool {
        self.pane.menu
            || self.pane.panel.open
            || self.pane.mode != Mode::Read
            || (self.pane.shelf.open && self.pane.doc.is_some())
            || self.split()
    }

    /// Two panes are shown.
    fn split(&self) -> bool {
        self.other.is_some()
    }

    /// The library screen is up in the focused pane: over a document, or as
    /// the home screen without one.
    pub fn shelf_visible(&self) -> bool {
        self.shows_shelf(&self.pane)
    }

    fn shows_shelf(&self, pane: &Pane) -> bool {
        pane.shelf.open || (pane.doc.is_none() && self.home)
    }

    /// Some pane shows the library (cover textures are needed).
    fn any_shelf(&self) -> bool {
        self.shows_shelf(&self.pane) || self.other.as_ref().is_some_and(|o| self.shows_shelf(o))
    }

    /// Nothing is open or being opened and the library is off: the old flow,
    /// where the app lives in the system file picker.
    fn picker_home(&self) -> bool {
        !self.home && self.pane.doc.is_none() && self.pane.pending.is_none() && !self.split()
    }

    /// Something is moving: render continuously.
    pub fn animating(&self) -> bool {
        self.pane_animating() || (self.split() && self.other_busy.0) || (self.any_shelf() && self.covers.uploading())
    }

    /// Something is moving in the focused pane.
    fn pane_animating(&self) -> bool {
        self.pane.fling.is_some()
            || self.pane.anim.is_some()
            || self.pane.panel.fling.is_some()
            || self.pane.overlay_t != self.overlay_target()
            || self.pane.indicator_t != self.indicator_target()
            || self.pane.panel.t != if self.pane.panel.open { 1.0 } else { 0.0 }
            || !self.pane.uploads.is_empty()
            || self.pane.shelf.fling.is_some()
    }

    pub fn needs_redraw(&self) -> bool {
        self.dirty || self.animating()
    }

    /// Next time `update` must run even without events.
    pub fn next_deadline(&self) -> Option<f64> {
        match (self.pane_deadline(), self.other_busy.1.filter(|_| self.split())) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn pane_deadline(&self) -> Option<f64> {
        let mut t: Option<f64> = None;
        let mut add = |v: f64| t = Some(t.map_or(v, |c: f64| c.min(v)));
        if let Some((at, _, _)) = self.pane.tap_at {
            add(at);
        }
        if self.pane.overlay && self.pane.mode == Mode::Read && !self.pane.panel.open && !self.pane.menu {
            add(self.pane.overlay_until);
        }
        if self.pane.indicator_t > 0.0 {
            add(self.pane.activity + 1.0);
        }
        if let Some((_, until)) = &self.pane.toast {
            add(*until);
        }
        if let Some(d) = self.pane.search.debounce {
            add(d);
        }
        if let Some((_, since)) = &self.pane.shelf.opening {
            add(since + 0.3);
        }
        t
    }

    // ----- split view -------------------------------------------------------------------

    /// Exchanges the focused pane with the other one. Everything pane-specific
    /// works on `self.pane`, so this is how the other pane gets updated, drawn
    /// and fed its worker's messages (swap, work, swap back).
    fn swap_panes(&mut self) {
        let Some(mut o) = self.other.take() else { return };
        self.other_busy = (self.pane_animating(), self.pane_deadline());
        std::mem::swap(&mut self.pane, &mut o);
        self.other = Some(o);
    }

    /// Runs `f` with the other pane swapped in (nothing without split view).
    fn with_other(&mut self, f: impl FnOnce(&mut Self)) {
        if self.split() {
            self.swap_panes();
            f(self);
            self.swap_panes();
        }
    }

    /// Runs `f` with pane `id` swapped in, or with the focused pane if there is no such pane.
    fn with_pane(&mut self, id: Option<u64>, f: impl FnOnce(&mut Self)) {
        if id.is_some() && self.other.as_ref().is_some_and(|o| Some(o.id) == id) {
            self.swap_panes();
            f(self);
            self.swap_panes();
        } else {
            f(self);
        }
    }

    /// Gives the focus to the other pane (a touch started there).
    fn focus_other(&mut self, p: &mut dyn Platform) {
        self.pane.menu = false;
        if self.pane.mode != Mode::Read {
            self.set_mode(Mode::Read, p);
        }
        self.pane.gesture = Gesture::None;
        self.swap_panes();
        self.dirty = true;
    }

    /// The window rect (x, y, w, h) and insets of the pane in `slot`. Split
    /// view divides the window along its longer side, with a thin divider.
    fn slot_rect(&self, slot: u8) -> ([f32; 4], [f32; 4]) {
        let (w, h, i) = (self.win_w, self.win_h, self.win_insets);
        if !self.split() {
            return ([0.0, 0.0, w, h], i);
        }
        let gap = self.divider();
        if w >= h {
            let half = ((w - gap) / 2.0).floor();
            match slot {
                0 => ([0.0, 0.0, half, h], [i[0], i[1], 0.0, i[3]]),
                _ => ([half + gap, 0.0, w - half - gap, h], [0.0, i[1], i[2], i[3]]),
            }
        } else {
            let half = ((h - gap) / 2.0).floor();
            match slot {
                0 => ([0.0, 0.0, w, half], [i[0], i[1], i[2], 0.0]),
                _ => ([0.0, half + gap, w, h - half - gap], [i[0], 0.0, i[2], i[3]]),
            }
        }
    }

    fn divider(&self) -> f32 {
        self.dp(2.0).round()
    }

    /// Puts both panes where they belong in the window.
    fn place_panes(&mut self) {
        self.place_pane();
        self.with_other(|a| a.place_pane());
        self.dirty = true;
    }

    fn place_pane(&mut self) {
        if self.win_w <= 0.0 {
            return;
        }
        let (r, insets) = self.slot_rect(self.pane.slot);
        let pn = &mut self.pane;
        (pn.x0, pn.y0, pn.insets) = (r[0], r[1], insets);
        // About 8 screens of tiles per pane, a bit less each in split view.
        let lo = if self.other.is_some() { 64 << 20 } else { 96 << 20 };
        pn.tiles.budget = ((r[2] * r[3] * 4.0) as usize * 8).clamp(lo, 256 << 20);
        if r[2] != pn.w || r[3] != pn.vh {
            pn.w = r[2];
            self.update_dock(r[3]);
        }
    }

    /// The split view button: opens a second pane next to this one, or closes
    /// this pane when two are shown.
    fn toggle_split(&mut self, p: &mut dyn Platform) {
        if self.split() {
            let id = self.pane.id;
            return self.close_pane(id, p);
        }
        self.save_position();
        let id = self.next_id;
        self.next_id += 1;
        let tab = self.pane.shelf.tab;
        let mut shelf = Shelf::new(&self.library, "");
        shelf.tab = tab;
        self.pane.slot = 0;
        self.pane.menu = false;
        self.other = Some(Box::new(Pane::new(id, 1, self.sink.clone(), shelf)));
        // The new pane gets the focus and shows the library (or the file picker).
        self.focus_other(p);
        self.place_panes();
        self.pane.overlay = true;
        self.pane.overlay_until = self.now + OVERLAY_TIMEOUT;
        if !self.home {
            self.pick(p);
        }
    }

    /// Closes pane `id`; the other one takes the whole window.
    fn close_pane(&mut self, id: u64, p: &mut dyn Platform) {
        if !self.split() {
            return;
        }
        if self.pane.id != id {
            self.swap_panes();
        }
        if self.pane.mode != Mode::Read {
            self.set_mode(Mode::Read, p);
        }
        self.save_position();
        self.pane.worker.close();
        if self.opener == Some(id) {
            self.opener = None;
        }
        let Some(rest) = self.other.take() else { return };
        let closed = std::mem::replace(&mut self.pane, *rest);
        self.retired.push(closed);
        self.pane.slot = 0;
        self.place_panes();
        self.invalidate();
    }

    /// Opens the file picker for the focused pane.
    fn pick(&mut self, p: &mut dyn Platform) {
        self.opener = Some(self.pane.id);
        p.open_picker();
    }

    /// Opens `uri` in the focused pane.
    fn open_uri(&mut self, uri: &str, p: &mut dyn Platform) {
        self.opener = Some(self.pane.id);
        p.open_document(uri);
    }

    // ----- geometry ---------------------------------------------------------------------

    fn bar_h(&self) -> f32 {
        self.dp(56.0)
    }

    fn bar_bottom(&self) -> f32 {
        self.pane.insets[1] + self.bar_h()
    }

    /// Width of the contents panel: the dock while docked, else the drawer.
    fn panel_w(&self) -> f32 {
        if self.docked() {
            match self.pane.gesture {
                Gesture::PanelResize { w, moved: true, .. } => w,
                _ => self.pane.ox,
            }
        } else {
            self.dp(360.0).min(self.pane.w * 0.86)
        }
    }

    fn docked(&self) -> bool {
        self.pane.ox > 0.0
    }

    /// Width the docked panel should have now, if it should be docked at all:
    /// pinned, the document has chapters and the window leaves room to read.
    fn dock_w(&self) -> Option<f32> {
        if !self.pin_chapters || !self.pane.doc.as_ref().is_some_and(|d| !d.outline.is_empty()) {
            return None;
        }
        let (lo, hi) = self.dock_range()?;
        let want =
            if self.chapters_w > 0 { self.dp(self.chapters_w as f32) } else { (self.pane.w * 0.32).clamp(self.dp(220.0), self.dp(340.0)) };
        Some(want.clamp(lo, hi).round())
    }

    /// Widths (px) the docked panel may take in this window: at least 220 dp,
    /// leaving at least 360 dp (and 40% of the window) for the document.
    fn dock_range(&self) -> Option<(f32, f32)> {
        let lo = self.dp(220.0);
        let hi = (self.pane.w - self.dp(360.0)).min(self.pane.w * 0.6);
        (hi >= lo).then_some((lo, hi))
    }

    /// Pin button in the panel header (pane coordinates, panel fully shown).
    fn pin_rect(&self) -> [f32; 4] {
        let b = self.dp(48.0);
        [self.panel_w() - b - self.dp(4.0), self.pane.insets[1] + self.dp(28.0) - b / 2.0, b, b]
    }

    fn panel_top(&self) -> f32 {
        self.pane.insets[1] + self.dp(56.0)
    }

    fn row_h(&self) -> f64 {
        self.dp(48.0) as f64
    }

    fn panel_max_scroll(&self) -> f64 {
        let n = self.pane.doc.as_ref().map_or(0, |d| d.outline.len()) as f64;
        (n * self.row_h() - (self.pane.vh - self.pane.insets[3] - self.panel_top()) as f64).max(0.0)
    }

    fn overlay_target(&self) -> f32 {
        if self.overlay_visible() {
            1.0
        } else {
            0.0
        }
    }

    fn overlay_visible(&self) -> bool {
        self.pane.overlay || self.pane.menu || self.pane.mode != Mode::Read || self.pane.doc.is_none() || self.shelf_visible()
    }

    fn indicator_target(&self) -> f32 {
        if self.now - self.pane.activity < 1.0 {
            1.0
        } else {
            0.0
        }
    }

    fn layout(&self) -> Option<&Layout> {
        self.pane.doc.as_ref().and_then(|d| d.layout.as_ref())
    }

    fn current_page(&self) -> usize {
        match self.layout() {
            Some(l) => l.page_at(self.pane.view.y + self.pane.vh as f64 / self.pane.view.zoom / 2.0),
            None => 0,
        }
    }

    fn position(&self) -> Option<Position> {
        let l = self.layout()?;
        let p = l.page_at(self.pane.view.y.max(0.0));
        Some(Position {
            page: p as u32,
            frac: ((self.pane.view.y - l.tops[p]) / l.heights[p]).clamp(0.0, 1.0),
            zoom: self.pane.view.zoom,
            x: self.pane.view.x / l.width,
        })
    }

    fn clamp_view(&self, v: View) -> View {
        match self.layout() {
            Some(l) => v.clamped(l, self.pane.vw as f64, self.pane.vh as f64),
            None => v,
        }
    }

    fn set_view(&mut self, v: View) {
        let v = self.clamp_view(v);
        if v != self.pane.view {
            self.pane.view = v;
            self.view_changed();
        }
    }

    fn view_changed(&mut self) {
        self.pane.requests_dirty = true;
        self.dirty = true;
    }

    fn touch_activity(&mut self) {
        self.pane.activity = self.now;
    }

    fn animate_to(&mut self, to: View, dur: f64) {
        let to = self.clamp_view(to);
        self.pane.fling = None;
        self.pane.anim = Some(Anim { from: self.pane.view, to, start: self.now, dur });
        self.dirty = true;
    }

    // ----- lifecycle --------------------------------------------------------------------

    pub fn resize(&mut self, w: u32, h: u32) {
        let (w, h) = (w.max(1) as f32, h.max(1) as f32);
        if w == self.win_w && h == self.win_h {
            return;
        }
        (self.win_w, self.win_h) = (w, h);
        self.place_panes();
    }

    /// Docks or undocks the contents panel as the preference, the document and
    /// the pane width require, resizing the document viewport to match.
    fn update_dock(&mut self, h: f32) {
        if self.pane.w <= 0.0 {
            return;
        }
        let was = self.docked();
        self.pane.ox = self.dock_w().unwrap_or(0.0);
        self.set_viewport(self.pane.w - self.pane.ox, h);
        if self.docked() && !was {
            self.pane.panel.open = false;
            self.pane.panel.t = 0.0;
            self.pane.panel.fling = None;
            self.pane.panel.follow = self.current_chapter();
            if let Some(c) = self.pane.panel.follow {
                self.center_chapter(c);
            }
        }
        self.dirty = true;
    }

    /// Sets the document viewport size (px), rebuilding the layout on width changes.
    fn set_viewport(&mut self, w: f32, h: f32) {
        if w == self.pane.vw && h == self.pane.vh {
            return;
        }
        let width_changed = w != self.pane.vw;
        let pos = self.position();
        self.pane.vw = w;
        self.pane.vh = h;
        if width_changed {
            self.pane.clear_tiles = true;
            if let Some(d) = self.pane.doc.as_mut() {
                if d.layout.is_some() {
                    d.layout = None;
                    d.restore = pos;
                }
            }
        }
        self.ensure_layout();
        self.pane.view = self.clamp_view(self.pane.view);
        self.view_changed();
    }

    fn ensure_layout(&mut self) {
        if self.pane.vw <= 0.0 {
            return;
        }
        let Some(d) = self.pane.doc.as_mut() else { return };
        if d.layout.is_some() {
            return;
        }
        let l = Layout::new(&d.sizes, self.pane.vw as f64, self.spread);
        let view = match d.restore.take() {
            Some(p) => View { zoom: p.zoom.clamp(MIN_ZOOM, MAX_ZOOM), x: p.x * l.width, y: l.y_of(p.page as usize, p.frac) },
            None => View::default(),
        };
        d.layout = Some(l);
        self.pane.anim = None;
        self.pane.fling = None;
        self.pane.view = self.clamp_view(view);
        self.pane.render_level = layout::level_for_zoom(self.pane.view.zoom);
        self.view_changed();
    }

    /// Remembers the reading position of the focused pane's document.
    fn save_position(&mut self) {
        if let Some(pos) = self.position() {
            if let Some(d) = &self.pane.doc {
                let uri = d.uri.clone();
                self.store.set(&uri, pos);
            }
        }
        self.store.save();
    }

    /// Remembers the reading positions of both panes.
    pub fn save_positions(&mut self) {
        self.with_other(|a| a.save_position());
        self.save_position();
    }

    pub fn shutdown(&mut self, mut gpu: Option<&mut dyn Gpu>) {
        self.save_positions();
        self.pane.worker.close();
        if let Some(o) = &self.other {
            o.worker.close();
        }
        if let Some(gpu) = gpu.as_deref_mut() {
            self.pane.tiles.clear(gpu);
            if let Some(o) = &mut self.other {
                o.tiles.clear(gpu);
            }
        }
        self.drop_retired(gpu);
    }

    /// Frees what closed panes left behind (their tiles; their workers stop).
    fn drop_retired(&mut self, mut gpu: Option<&mut dyn Gpu>) {
        for mut pane in self.retired.drain(..) {
            if let Some(gpu) = gpu.as_deref_mut() {
                pane.tiles.clear(gpu);
            }
            for u in pane.uploads.drain(..) {
                pane.worker.recycle(u.pixels);
            }
        }
    }

    /// Forces a redraw and a fresh tile request (e.g. after the window came back).
    pub fn invalidate(&mut self) {
        self.dirty = true;
        self.pane.requests_dirty = true;
        if let Some(o) = &mut self.other {
            o.requests_dirty = true;
        }
    }

    /// The window went away: keep only what is on screen.
    pub fn trim(&mut self, gpu: &mut dyn Gpu) {
        self.covers.clear_gpu(gpu);
        self.drop_retired(Some(gpu));
        self.trim_pane(gpu);
        self.with_other(|a| a.trim_pane(gpu));
    }

    fn trim_pane(&mut self, gpu: &mut dyn Gpu) {
        let Some(l) = self.layout() else { return };
        let v = self.pane.view;
        let range = l.range(v.y, v.y + self.pane.vh as f64 / v.zoom);
        let level = self.pane.render_level;
        self.pane.tiles.retain(gpu, |k| range.contains(&(k.page as usize)) && (k.level == level || k.is_thumb()));
    }

    /// The GL context was lost; every texture is gone.
    pub fn gpu_lost(&mut self) {
        self.pane.tiles.forget();
        if let Some(o) = &mut self.other {
            o.tiles.forget();
        }
        for pn in &mut self.retired {
            pn.tiles.forget();
        }
        self.covers.forget_gpu();
        self.text.forget_gpu();
        self.invalidate();
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.pane.toast = Some((msg.into(), self.now + 2.5));
        self.dirty = true;
    }

    // ----- events -----------------------------------------------------------------------

    pub fn handle(&mut self, ev: Event, now: f64, p: &mut dyn Platform) {
        self.now = now;
        self.dirty = true;
        match ev {
            Event::Insets(i) => {
                self.win_insets = i;
                self.place_panes();
            }
            Event::Touch { action, index, pointers, time } => self.touch(action, index, pointers, time, p),
            Event::Wheel { x, y, delta } => {
                let there = self.other.as_ref().is_some_and(|o| o.contains(x, y));
                if there {
                    self.with_other(|a| a.wheel(delta));
                } else {
                    self.wheel(delta);
                }
            }
            Event::Key(_) if self.shelf_visible() => {}
            Event::Key(code) => self.key(code),
            Event::Back => {
                if !self.back(p) && self.split() {
                    // Leave split view: the pane opened last (right / bottom) closes.
                    let id = if self.pane.slot == 1 { self.pane.id } else { self.other.as_ref().map_or(0, |o| o.id) };
                    self.close_pane(id, p);
                }
            }
            Event::Open { fd, uri, name } => {
                let opener = self.opener.take();
                self.with_pane(opener, |a| a.open_fd(fd, uri, name, now, p));
            }
            Event::OpenFailed(msg) => {
                crate::log_w!("open failed: {msg}");
                let opener = self.opener.take();
                self.with_pane(opener, |a| {
                    a.pane.shelf.opening = None;
                    a.toast("Can't open this file");
                });
                if self.picker_home() {
                    self.pick(p);
                }
            }
            Event::PickerCancelled => {
                let opener = self.opener.take();
                let empty = |pn: &Pane| pn.doc.is_none() && pn.pending.is_none();
                if self.picker_home() {
                    p.exit();
                } else if !self.home && self.split() {
                    // A new split pane without the library: no file chosen, no pane.
                    let id = opener.unwrap_or(self.pane.id);
                    let gone =
                        if self.pane.id == id { empty(&self.pane) } else { self.other.as_ref().is_some_and(|o| o.id == id && empty(o)) };
                    if gone {
                        self.close_pane(id, p);
                    }
                }
            }
            Event::NothingToOpen => {
                if self.picker_home() {
                    self.pick(p);
                }
            }
            Event::FolderPicked { uri, name } => self.folder_picked(&uri, &name, p),
            Event::FolderScanned { uri, books, ok } => self.folder_scanned(&uri, books, ok),
            Event::CoverFd { key, fd } => self.cover_fd(key, fd),
            Event::Text { text, cursor } => {
                self.pane.cursor = utf16_to_char_index(&text, cursor);
                if text != self.pane.field {
                    self.pane.field = text;
                    if self.pane.mode == Mode::Search {
                        self.pane.search.debounce = Some(now + 0.25);
                    }
                }
            }
            Event::ImeAction => self.ime_action(p),
            Event::Pause => self.save_positions(),
            Event::Worker(msg) => {
                // Messages about a document go to the pane that has it.
                let doc = match &msg {
                    Msg::Opened { doc, .. }
                    | Msg::NeedPassword { doc, .. }
                    | Msg::OpenFailed { doc, .. }
                    | Msg::Outline { doc, .. }
                    | Msg::Tile { doc, .. }
                    | Msg::SearchHits { doc, .. }
                    | Msg::SearchProgress { doc, .. } => Some(*doc),
                    Msg::Cover { .. } | Msg::CoverFailed { .. } => None,
                };
                let there = doc.is_some_and(|d| !self.pane.owns(d) && self.other.as_ref().is_some_and(|o| o.owns(d)));
                if there {
                    self.with_other(|a| a.worker_msg(msg, p));
                } else {
                    self.worker_msg(msg, p);
                }
            }
        }
    }

    fn wheel(&mut self, delta: f32) {
        if self.shelf_visible() {
            return self.shelf_scroll_by(-(delta as f64) * self.dp(64.0) as f64);
        }
        let dy = -(delta as f64) * self.dp(64.0) as f64 / self.pane.view.zoom;
        self.pane.anim = None;
        self.pane.fling = None;
        self.pane.scroll_dir = if dy >= 0.0 { 1.0 } else { -1.0 };
        self.set_view(View { y: self.pane.view.y + dy, ..self.pane.view });
        self.touch_activity();
    }

    /// A document was opened for the focused pane: hand it to its worker.
    fn open_fd(&mut self, fd: i32, uri: String, name: String, now: f64, p: &mut dyn Platform) {
        self.save_position();
        let id = self.next_id;
        self.next_id += 1;
        self.pane.worker.open(id, fd);
        if self.pane.mode == Mode::Password {
            self.set_mode(Mode::Read, p);
        }
        self.pane.pending = Some(Pending { id, uri, name, since: now });
    }

    fn worker_msg(&mut self, msg: Msg, p: &mut dyn Platform) {
        match msg {
            Msg::Opened { doc, sizes } => {
                let Some(pending) = self.pane.pending.take_if(|pd| pd.id == doc) else { return };
                let restore = self.store.get(&pending.uri);
                self.store.opened(&pending.uri, &pending.name, sizes.len() as u32);
                self.opened_cover(doc, &pending.uri);
                self.pane.shelf.open = false;
                self.pane.shelf.opening = None;
                self.pane.shelf.stale = true;
                self.pane.doc =
                    Some(Doc { id: doc, uri: pending.uri, name: pending.name, sizes, outline: Vec::new(), layout: None, restore });
                self.pane.clear_tiles = true;
                self.pane.sent.clear();
                self.pane.wanted.clear();
                self.pane.search = SearchState::default();
                self.pane.panel = Panel::default();
                if self.pane.mode != Mode::Read {
                    self.set_mode(Mode::Read, p);
                }
                self.pane.view = View::default();
                self.ensure_layout();
                self.pane.overlay = true;
                self.pane.overlay_until = self.now + OVERLAY_TIMEOUT;
                self.touch_activity();
            }
            Msg::NeedPassword { doc, wrong } => {
                if self.pane.pending.as_ref().is_some_and(|pd| pd.id == doc) {
                    self.pane.field.clear();
                    self.pane.cursor = 0;
                    self.pane.mode = Mode::Password;
                    p.show_keyboard(KB_PASSWORD, "");
                    self.keyboard = true;
                    if wrong {
                        self.toast("Wrong password");
                    }
                }
            }
            Msg::OpenFailed { doc, error } => {
                if self.pane.pending.take_if(|pd| pd.id == doc).is_some() {
                    crate::log_w!("cannot open: {error}");
                    self.pane.shelf.opening = None;
                    self.toast("Can't open this file");
                    if self.picker_home() {
                        self.pick(p);
                    }
                }
            }
            Msg::Outline { doc, items } => {
                if let Some(d) = self.pane.doc.as_mut().filter(|d| d.id == doc) {
                    d.outline = items;
                    self.update_dock(self.pane.vh);
                }
            }
            Msg::Tile { doc, key, w, h, pixels } => {
                if self.pane.doc.as_ref().is_some_and(|d| d.id == doc) && !self.pane.tiles.contains(&key) && self.pane.in_flight.insert(key)
                {
                    self.pane.uploads.push_back(Upload { doc, key, w, h, pixels });
                } else {
                    self.pane.worker.recycle(pixels);
                }
            }
            Msg::SearchHits { doc, gen, page, hits } => {
                if self.pane.doc.as_ref().is_some_and(|d| d.id == doc) && gen == self.pane.search.gen {
                    self.add_hits(page, hits);
                }
            }
            Msg::SearchProgress { doc, gen, scanned, total } => {
                if self.pane.doc.as_ref().is_some_and(|d| d.id == doc) && gen == self.pane.search.gen {
                    self.pane.search.scanned = scanned;
                    self.pane.search.total = total;
                    if scanned >= total {
                        self.pane.search.running = false;
                    }
                }
            }
            Msg::Cover { key, w, h, pixels } => self.cover_ready(key, w, h, pixels),
            Msg::CoverFailed { key, missing } => self.cover_failed(key, missing),
        }
    }

    fn key(&mut self, code: i32) {
        let vh = self.pane.vh as f64 / self.pane.view.zoom;
        let vw = self.pane.vw as f64 / self.pane.view.zoom;
        let v = self.pane.view;
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
        self.pane.scroll_dir = if to.y >= v.y { 1.0 } else { -1.0 };
        self.animate_to(to, 0.18);
        self.touch_activity();
    }

    fn back(&mut self, p: &mut dyn Platform) -> bool {
        if self.pane.menu {
            self.pane.menu = false;
            return true;
        }
        if self.pane.panel.open {
            self.pane.panel.open = false;
            return true;
        }
        if self.pane.mode != Mode::Read {
            self.set_mode(Mode::Read, p);
            return true;
        }
        if self.pane.shelf.open && self.pane.doc.is_some() {
            self.hide_shelf();
            return true;
        }
        false
    }

    fn set_mode(&mut self, mode: Mode, p: &mut dyn Platform) {
        let old = self.pane.mode;
        if old == Mode::Search && mode != Mode::Search {
            self.pane.search.text = self.pane.field.clone();
            self.pane.worker.cancel_search();
            let gen = self.pane.search.gen + 1;
            let text = std::mem::take(&mut self.pane.search.text);
            self.pane.search = SearchState { gen, text, ..Default::default() };
        }
        if old == Mode::Password && mode != Mode::Password {
            self.pane.pending = None;
            self.pane.shelf.opening = None;
            if self.picker_home() {
                self.pick(p);
            }
        }
        self.pane.mode = mode;
        self.pane.panel.open = false;
        self.pane.menu = false;
        match mode {
            Mode::Read => {
                if self.keyboard {
                    p.hide_keyboard();
                    self.keyboard = false;
                }
                self.pane.overlay = true;
                self.pane.overlay_until = self.now + OVERLAY_TIMEOUT;
            }
            Mode::Search => {
                self.pane.field = self.pane.search.text.clone();
                self.pane.cursor = self.pane.field.chars().count();
                p.show_keyboard(KB_TEXT, &self.pane.field);
                self.keyboard = true;
                if !self.pane.field.is_empty() {
                    self.pane.search.query.clear();
                    self.start_search();
                }
            }
            Mode::Goto => {
                self.pane.field.clear();
                self.pane.cursor = 0;
                p.show_keyboard(KB_NUMBER, "");
                self.keyboard = true;
            }
            Mode::Password => {}
        }
    }

    fn ime_action(&mut self, p: &mut dyn Platform) {
        match self.pane.mode {
            Mode::Search => {
                if self.pane.search.debounce.is_some() || search::fold_query(&self.pane.field) != self.pane.search.query {
                    self.start_search();
                } else {
                    self.step_match(1);
                }
                p.hide_keyboard();
                self.keyboard = false;
            }
            Mode::Goto => {
                let n = self.pane.doc.as_ref().map_or(0, |d| d.sizes.len());
                match self.pane.field.trim().parse::<usize>() {
                    Ok(page) if page >= 1 && page <= n => {
                        self.jump_to(page - 1, 0.0);
                        self.set_mode(Mode::Read, p);
                        self.pane.overlay = false;
                    }
                    _ => self.toast(format!("Enter a page from 1 to {n}")),
                }
            }
            Mode::Password => {
                if let Some(pd) = &self.pane.pending {
                    self.pane.worker.password(pd.id, self.pane.field.clone());
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
        self.pane.anim = None;
        self.pane.fling = None;
        self.set_view(View { y, ..self.pane.view });
        self.touch_activity();
    }

    // ----- search -----------------------------------------------------------------------

    fn start_search(&mut self) {
        self.pane.search.debounce = None;
        let q = search::fold_query(&self.pane.field);
        if q == self.pane.search.query && !self.pane.search.hits.is_empty() {
            return;
        }
        self.pane.search.gen += 1;
        self.pane.search.query = q.clone();
        self.pane.search.hits.clear();
        self.pane.search.current = None;
        self.pane.search.scanned = 0;
        self.pane.search.auto_jump = true;
        let (id, n) = match &self.pane.doc {
            Some(d) => (d.id, d.sizes.len() as u32),
            None => return,
        };
        self.pane.search.total = n;
        self.pane.search.running = !q.is_empty();
        let start = self.current_page() as u32;
        self.pane.worker.search(id, self.pane.search.gen, q, start, n);
    }

    fn add_hits(&mut self, page: u32, hits: Vec<Vec<[f32; 4]>>) {
        let idx = self.pane.search.hits.partition_point(|m| m.page < page);
        let n = hits.len();
        let new = hits.into_iter().map(|rects| Match { page, rects });
        self.pane.search.hits.splice(idx..idx, new);
        match self.pane.search.current {
            Some(c) if c >= idx => self.pane.search.current = Some(c + n),
            Some(_) => {}
            None => {
                self.pane.search.current = Some(idx);
                if self.pane.search.auto_jump {
                    self.pane.search.auto_jump = false;
                    self.go_to_match(idx);
                }
            }
        }
    }

    fn step_match(&mut self, dir: i64) {
        let n = self.pane.search.hits.len() as i64;
        if n == 0 {
            return;
        }
        let c = self.pane.search.current.map_or(0, |c| (c as i64 + dir).rem_euclid(n)) as usize;
        self.pane.search.current = Some(c);
        self.pane.search.auto_jump = false;
        self.go_to_match(c);
    }

    fn go_to_match(&mut self, i: usize) {
        let Some(m) = self.pane.search.hits.get(i) else { return };
        let Some(l) = self.layout() else { return };
        let page = m.page as usize;
        if page >= l.len() {
            return;
        }
        let r = m.rects.iter().fold([1f32, 1.0, 0.0, 0.0], |a, r| [a[0].min(r[0]), a[1].min(r[1]), a[2].max(r[2]), a[3].max(r[3])]);
        let doc_y = l.tops[page] + r[1] as f64 * l.heights[page];
        let doc_x = l.xs[page] + (r[0] + r[2]) as f64 / 2.0 * l.widths[page];
        let z = self.pane.view.zoom;
        let (vw, vh) = (self.pane.vw as f64 / z, self.pane.vh as f64 / z);
        let top = self.bar_bottom() as f64 / z;
        let mut to = self.pane.view;
        // Only scroll if the match is not comfortably visible.
        if doc_y < self.pane.view.y + top || doc_y > self.pane.view.y + vh * 0.8 {
            to.y = doc_y - vh * 0.3;
        }
        if z > 1.0 && (doc_x < self.pane.view.x || doc_x > self.pane.view.x + vw) {
            to.x = doc_x - vw / 2.0;
        }
        let to = self.clamp_view(to);
        if (to.y - self.pane.view.y).abs() * z < self.pane.vh as f64 * 2.5 {
            self.animate_to(to, 0.25);
        } else {
            self.pane.anim = None;
            self.set_view(to);
        }
        self.touch_activity();
    }

    // ----- touch ------------------------------------------------------------------------

    fn touch(&mut self, action: i32, index: usize, pointers: Vec<(i32, f32, f32)>, time: f64, p: &mut dyn Platform) {
        self.pointers = pointers;
        if action == ACTION_DOWN {
            // A touch in the other half of the split view moves the focus there.
            let (x, y) = self.pointers.get(index).map_or((0.0, 0.0), |&(_, x, y)| (x, y));
            if self.other.as_ref().is_some_and(|o| o.contains(x, y)) && !self.pane.contains(x, y) {
                self.focus_other(p);
            }
        }
        // From here on everything is in the coordinates of the focused pane.
        let (x0, y0) = (self.pane.x0, self.pane.y0);
        if x0 != 0.0 || y0 != 0.0 {
            for p in &mut self.pointers {
                (p.1, p.2) = (p.1 - x0, p.2 - y0);
            }
        }
        if self.pane.menu {
            return self.menu_touch(action, index, p);
        }
        if self.shelf_visible() && self.pane.mode != Mode::Password {
            // The library works in pane coordinates.
            return self.shelf_touch(action, index, time, p);
        }
        // Everything but the docked panel works in document-viewport coordinates.
        if self.pane.ox != 0.0 {
            for p in &mut self.pointers {
                p.1 -= self.pane.ox;
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
                self.pane.gesture = Gesture::None;
                self.pane.vel.reset();
            }
            _ => {}
        }
    }

    fn on_down(&mut self, x: f32, y: f32, time: f64) {
        self.pane.vel.reset();
        self.pane.vel.add(time, x as f64, y as f64);
        let wx = x + self.pane.ox;
        // Grab zone around the docked panel's edge (below the top bar).
        if self.docked() && y > self.panel_top() && wx > self.pane.ox - self.dp(10.0) && wx < self.pane.ox + self.dp(14.0) {
            self.pane.panel.fling = None;
            self.pane.gesture = Gesture::PanelResize { w: self.pane.ox, grab: wx - self.pane.ox, moved: false };
            return;
        }
        if self.pane.panel.open || (self.docked() && wx < self.pane.ox) {
            self.pane.panel.fling = None;
            let on_panel = wx < self.panel_w();
            self.pane.gesture = if on_panel { Gesture::Panel { x: wx, y, start: y, moved: false } } else { Gesture::PanelOutside };
            return;
        }
        let controls = self.pane.overlay_t > 0.5 || self.pane.mode != Mode::Read;
        if controls {
            if let Some(b) = self.pane.buttons.iter().rev().find(|b| inside(b.r, x, y)).copied() {
                self.pane.gesture = Gesture::Button { hit: b.hit, r: b.r, inside: true };
                return;
            }
        }
        let scroll_alpha = self.pane.overlay_t.max(self.pane.indicator_t);
        if scroll_alpha > 0.3 && x > self.pane.vw - self.pane.insets[2] - self.dp(36.0) && self.scrollable() {
            self.pane.gesture = Gesture::Scrollbar;
            self.pane.fling = None;
            self.pane.anim = None;
            self.scrollbar_to(y);
            return;
        }
        let stopped = self.pane.fling.is_some() || self.pane.anim.is_some();
        self.pane.fling = None;
        self.pane.anim = None;
        self.pane.gesture = Gesture::Pending { x, y, t: time, stopped };
    }

    fn on_pointer_down(&mut self) {
        if self.pointers.len() < 2 {
            return;
        }
        match self.pane.gesture {
            Gesture::Pending { .. } | Gesture::Drag { .. } | Gesture::Pinch { .. } => {
                let (_, x0, y0) = self.pointers[0];
                let (_, x1, y1) = self.pointers[1];
                let d0 = ((x1 - x0) as f64).hypot((y1 - y0) as f64).max(1.0);
                let mid = ((x0 + x1) as f64 / 2.0, (y0 + y1) as f64 / 2.0);
                self.pane.gesture = Gesture::Pinch { d0, z0: self.pane.view.zoom, doc: self.pane.view.to_doc(mid.0, mid.1) };
                self.pane.tap_at = None;
                self.pane.fling = None;
                self.pane.anim = None;
            }
            _ => {}
        }
    }

    fn on_pointer_up(&mut self, index: usize) {
        if let Gesture::Pinch { .. } | Gesture::Drag { .. } = self.pane.gesture {
            // Continue as a drag with a remaining finger.
            if let Some(&(id, x, y)) = self.pointers.iter().enumerate().find(|(i, _)| *i != index).map(|(_, p)| p) {
                self.pane.gesture = Gesture::Drag { id, x, y };
                self.pane.vel.reset();
            }
            self.pane.requests_dirty = true;
        }
    }

    fn on_move(&mut self, time: f64) {
        let slop = self.dp(8.0);
        match self.pane.gesture {
            Gesture::Pending { x, y, .. } => {
                let Some(&(id, cx, cy)) = self.pointers.first() else { return };
                self.pane.vel.add(time, cx as f64, cy as f64);
                if (cx - x).hypot(cy - y) > slop {
                    self.pane.gesture = Gesture::Drag { id, x: cx, y: cy };
                    if self.pane.mode == Mode::Read && self.pane.overlay {
                        self.pane.overlay = false;
                    }
                    self.pane.tap_at = None;
                    // Apply the movement past the slop immediately.
                    self.drag_by(cx - x, cy - y);
                }
            }
            Gesture::Drag { id, x, y } => {
                let Some(&(_, cx, cy)) = self.pointers.iter().find(|p| p.0 == id) else { return };
                self.pane.vel.add(time, cx as f64, cy as f64);
                self.pane.gesture = Gesture::Drag { id, x: cx, y: cy };
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
                    self.pane.gesture = Gesture::Button { hit, r, inside: inside(r, cx, cy) };
                }
            }
            Gesture::Scrollbar => {
                if let Some(&(_, _, cy)) = self.pointers.first() {
                    self.scrollbar_to(cy);
                }
            }
            Gesture::Panel { x, y, start, moved } => {
                let Some(&(_, _, cy)) = self.pointers.first() else { return };
                self.pane.vel.add(time, 0.0, cy as f64);
                let moved = moved || (cy - start).abs() > slop;
                if moved {
                    let max = self.panel_max_scroll();
                    self.pane.panel.scroll = (self.pane.panel.scroll - (cy - y) as f64).clamp(0.0, max);
                }
                self.pane.gesture = Gesture::Panel { x, y: cy, start, moved };
            }
            Gesture::PanelResize { w, grab, moved } => {
                let Some(&(_, cx, _)) = self.pointers.first() else { return };
                let edge = cx + self.pane.ox - grab;
                let moved = moved || (edge - self.pane.ox).abs() > slop;
                let w = match self.dock_range() {
                    Some((lo, hi)) if moved => edge.clamp(lo, hi),
                    _ => w,
                };
                self.pane.gesture = Gesture::PanelResize { w, grab, moved };
            }
            Gesture::PanelOutside | Gesture::Shelf { .. } | Gesture::ShelfTabs { .. } | Gesture::None => {}
        }
    }

    fn drag_by(&mut self, dx: f32, dy: f32) {
        let z = self.pane.view.zoom;
        if dy != 0.0 {
            self.pane.scroll_dir = if dy < 0.0 { 1.0 } else { -1.0 };
        }
        self.set_view(View { x: self.pane.view.x - dx as f64 / z, y: self.pane.view.y - dy as f64 / z, ..self.pane.view });
        self.touch_activity();
    }

    fn on_up(&mut self, x: f32, y: f32, time: f64, p: &mut dyn Platform) {
        let gesture = std::mem::replace(&mut self.pane.gesture, Gesture::None);
        match gesture {
            Gesture::Pending { t, stopped, .. } => {
                if stopped {
                    return;
                }
                let dbl = self.pane.last_tap.is_some_and(|(lt, lx, ly)| t - lt < DOUBLE_TAP_GAP && (x - lx).hypot(y - ly) < self.dp(40.0));
                if dbl {
                    self.pane.last_tap = None;
                    self.pane.tap_at = None;
                    self.double_tap(x, y);
                } else {
                    self.pane.last_tap = Some((time, x, y));
                    self.pane.tap_at = Some((self.now + TAP_TIMEOUT, x, y));
                }
            }
            Gesture::Drag { .. } => {
                let (vx, vy) = self.pane.vel.velocity(time);
                let (vx, vy) = if self.pane.view.zoom > 1.0001 { (vx, vy) } else { (0.0, vy) };
                self.pane.fling = Fling::new(self.now, vx, vy, self.density as f64);
            }
            Gesture::Pinch { .. } => {
                self.pane.requests_dirty = true;
            }
            Gesture::Button { hit, r, .. } => {
                if inside(r, x, y) {
                    self.press(hit, p);
                }
            }
            Gesture::Scrollbar => {}
            Gesture::Panel { x: sx, start, moved, .. } => {
                if moved {
                    let (_, vy) = self.pane.vel.velocity(time);
                    self.pane.panel.fling = Fling::new(self.now, 0.0, vy, self.density as f64);
                } else if start < self.panel_top() {
                    let r = self.pin_rect();
                    if inside(r, sx, start) && inside(r, x + self.pane.ox, y) {
                        self.toggle_pin();
                    }
                } else {
                    let row = ((y - self.panel_top()) as f64 + self.pane.panel.scroll) / self.row_h();
                    if row >= 0.0 && y >= self.panel_top() {
                        let item = self.pane.doc.as_ref().and_then(|d| d.outline.get(row as usize)).cloned();
                        if let Some(item) = item {
                            self.jump_to(item.page as usize, item.y.unwrap_or(0.0) as f64);
                            self.pane.panel.open = false;
                            self.pane.overlay = false;
                        }
                    }
                }
            }
            Gesture::PanelOutside => self.pane.panel.open = false,
            Gesture::PanelResize { w, moved, .. } => {
                if moved {
                    self.chapters_w = (w / self.density).round() as u32;
                    self.store.set_chapters_w(self.chapters_w);
                    self.update_dock(self.pane.vh);
                    self.with_other(|a| a.update_dock(a.pane.vh));
                }
            }
            Gesture::Shelf { .. } | Gesture::ShelfTabs { .. } | Gesture::None => {}
        }
    }

    fn single_tap(&mut self, _x: f32, _y: f32, p: &mut dyn Platform) {
        if self.keyboard && self.pane.mode != Mode::Read {
            p.hide_keyboard();
            self.keyboard = false;
            return;
        }
        if self.pane.mode == Mode::Read && self.pane.doc.is_some() {
            self.pane.overlay = !self.pane.overlay;
            self.pane.overlay_until = self.now + OVERLAY_TIMEOUT;
            self.dirty = true;
        }
    }

    fn double_tap(&mut self, x: f32, y: f32) {
        let target = if self.pane.view.zoom > 1.05 || self.pane.view.zoom < 0.95 { 1.0 } else { 2.5 };
        let to = self.pane.view.zoomed_at(target, x as f64, y as f64);
        self.animate_to(to, 0.22);
    }

    fn press(&mut self, hit: Hit, p: &mut dyn Platform) {
        self.pane.overlay_until = self.now + OVERLAY_TIMEOUT;
        match hit {
            Hit::Open if self.home => self.show_shelf(p),
            Hit::Open => self.pick(p),
            Hit::Settings => self.pane.menu = !self.pane.menu,
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
            Hit::MenuSpread => self.toggle_spread(),
            Hit::Split => self.toggle_split(p),
            Hit::FitWidth => self.fit_width(),
            Hit::Caffeine => {
                self.caffeine = !self.caffeine;
                self.store.set_caffeine(self.caffeine);
                self.toast(if self.caffeine { "Screen stays on" } else { "Screen turns off as usual" });
            }
            Hit::Close => self.set_mode(Mode::Read, p),
            Hit::Prev => self.step_match(-1),
            Hit::Next => self.step_match(1),
            Hit::Field => {
                let kb = match self.pane.mode {
                    Mode::Goto => KB_NUMBER,
                    Mode::Password => KB_PASSWORD,
                    _ => KB_TEXT,
                };
                p.show_keyboard(kb, &self.pane.field);
                self.keyboard = true;
            }
            Hit::Indicator => {
                if self.pane.mode == Mode::Read {
                    self.set_mode(Mode::Goto, p);
                }
            }
            Hit::Bar => {}
        }
    }

    /// Whether the view is zoomed away from the default fit-width size.
    fn zoomed(&self) -> bool {
        (self.pane.view.zoom - 1.0).abs() > 1e-3
    }

    /// Animates back to zoom 1 (rows span the viewport width), keeping the
    /// document line at the middle of the screen in place.
    fn fit_width(&mut self) {
        let mut to = self.pane.view.zoomed_at(1.0, self.pane.vw as f64 / 2.0, self.pane.vh as f64 / 2.0);
        to.x = 0.0;
        self.animate_to(to, 0.22);
    }

    /// Switches between one page and two pages per row, keeping the current page.
    fn toggle_spread(&mut self) {
        self.spread = !self.spread;
        self.store.set_spread(self.spread);
        self.relayout();
        self.with_other(|a| a.relayout());
        self.toast(if self.spread { "Two pages side by side" } else { "One page per row" });
    }

    /// Rebuilds the layout (and tiles) of the focused pane, keeping the current page.
    fn relayout(&mut self) {
        let pos = self.position().map(|p| Position { zoom: 1.0, x: 0.0, ..p });
        if let Some(d) = self.pane.doc.as_mut() {
            if d.layout.is_some() {
                d.layout = None;
                d.restore = pos;
            }
        }
        self.pane.clear_tiles = true;
        self.pane.sent.clear();
        self.pane.wanted.clear();
        self.ensure_layout();
    }

    fn open_panel(&mut self) {
        let Some(d) = &self.pane.doc else { return };
        if d.outline.is_empty() {
            self.toast("This document has no chapters");
            return;
        }
        let cur = self.current_chapter().unwrap_or(0);
        self.center_chapter(cur);
        self.pane.panel.open = true;
        self.pane.panel.fling = None;
    }

    /// Scrolls the contents list so chapter `i` is in the middle.
    fn center_chapter(&mut self, i: usize) {
        let visible = (self.pane.vh - self.pane.insets[3] - self.panel_top()) as f64;
        self.pane.panel.scroll = (i as f64 * self.row_h() - visible / 2.0 + self.row_h() / 2.0).clamp(0.0, self.panel_max_scroll());
    }

    /// Pins or unpins the contents panel; docks it right away if the window allows.
    fn toggle_pin(&mut self) {
        self.pin_chapters = !self.pin_chapters;
        self.store.set_pin_chapters(self.pin_chapters);
        if !self.pin_chapters {
            self.pane.panel.open = false;
        }
        self.update_dock(self.pane.vh);
        self.with_other(|a| a.update_dock(a.pane.vh));
        if self.pin_chapters && !self.docked() {
            self.toast("Contents will stay pinned when the window is wide enough");
        }
    }

    fn current_chapter(&self) -> Option<usize> {
        let d = self.pane.doc.as_ref()?;
        let l = d.layout.as_ref()?;
        let y = self.pane.view.y + self.pane.vh as f64 / self.pane.view.zoom * 0.15;
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
        self.layout().is_some_and(|l| l.total * self.pane.view.zoom > self.pane.vh as f64 * 1.5)
    }

    fn scrollbar_track(&self) -> (f32, f32) {
        let top = self.pane.insets[1] + self.dp(8.0);
        let bottom = self.pane.vh - self.pane.insets[3] - self.dp(8.0);
        (top, bottom.max(top + 1.0))
    }

    fn scrollbar_to(&mut self, y: f32) {
        let Some(l) = self.layout() else { return };
        let (top, bottom) = self.scrollbar_track();
        let thumb = self.dp(44.0);
        let f = ((y - top - thumb / 2.0) / (bottom - top - thumb).max(1.0)).clamp(0.0, 1.0) as f64;
        let max = (l.total - self.pane.vh as f64 / self.pane.view.zoom).max(0.0);
        self.set_view(View { y: f * max, ..self.pane.view });
        self.touch_activity();
    }

    // ----- per-frame update -------------------------------------------------------------

    pub fn update(&mut self, now: f64, p: &mut dyn Platform) {
        let dt = (now - self.last_update).clamp(0.0, 0.1) as f32;
        self.last_update = now;
        self.now = now;
        if self.last_scan.is_none() {
            self.scan_all(p);
        }

        self.with_other(|a| a.update_pane(now, dt, p));
        self.update_pane(now, dt, p);

        if self.caffeine_applied != Some(self.caffeine) {
            self.caffeine_applied = Some(self.caffeine);
            p.keep_screen_on(self.caffeine);
        }
        let bars = self.overlay_visible();
        if self.bars != Some(bars) {
            self.bars = Some(bars);
            p.system_bars(bars);
        }
    }

    fn update_pane(&mut self, now: f64, dt: f32, p: &mut dyn Platform) {
        if let Some((at, x, y)) = self.pane.tap_at {
            if now >= at {
                self.pane.tap_at = None;
                self.single_tap(x, y, p);
            }
        }

        if let Some(mut f) = self.pane.fling.take() {
            let ((dx, dy), running) = f.step(now);
            let z = self.pane.view.zoom;
            let want = View { x: self.pane.view.x - dx / z, y: self.pane.view.y - dy / z, ..self.pane.view };
            let got = self.clamp_view(want);
            if (got.x - want.x).abs() > 1e-6 {
                f.stop_axis(true);
            }
            if (got.y - want.y).abs() > 1e-6 {
                f.stop_axis(false);
            }
            if got != self.pane.view {
                self.pane.view = got;
                self.view_changed();
                self.touch_activity();
            }
            if running && !f.is_dead() {
                self.pane.fling = Some(f);
            }
        }

        if let Some(a) = &self.pane.anim {
            let u = ((now - a.start) / a.dur).clamp(0.0, 1.0);
            let e = ease_out(u);
            let (f, t) = (a.from, a.to);
            let zoom = (f.zoom.ln() + (t.zoom.ln() - f.zoom.ln()) * e).exp();
            let v = View { zoom, x: f.x + (t.x - f.x) * e, y: f.y + (t.y - f.y) * e };
            if u >= 1.0 {
                self.pane.anim = None;
            }
            let v = if u >= 1.0 { self.clamp_view(t) } else { v };
            if v != self.pane.view {
                self.pane.view = v;
                self.view_changed();
            }
        }

        if let Some(mut f) = self.pane.panel.fling.take() {
            let ((_, dy), running) = f.step(now);
            let max = self.panel_max_scroll();
            let s = self.pane.panel.scroll - dy;
            self.pane.panel.scroll = s.clamp(0.0, max);
            if running && s == self.pane.panel.scroll {
                self.pane.panel.fling = Some(f);
            }
            self.dirty = true;
        }

        if self.pane.overlay && self.pane.mode == Mode::Read && !self.pane.panel.open && !self.pane.menu && now >= self.pane.overlay_until {
            self.pane.overlay = false;
        }
        let step = dt / 0.15;
        let target = self.overlay_target();
        self.dirty |= approach(&mut self.pane.overlay_t, target, step);
        let target = self.indicator_target();
        self.dirty |= approach(&mut self.pane.indicator_t, target, dt / 0.3);
        let target = if self.pane.panel.open { 1.0 } else { 0.0 };
        self.dirty |= approach(&mut self.pane.panel.t, target, dt / 0.18);

        if self.pane.toast.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.pane.toast = None;
            self.dirty = true;
        }
        if self.pane.search.debounce.is_some_and(|d| now >= d) {
            self.start_search();
        }

        if self.docked() && self.pane.panel.fling.is_none() && !matches!(self.pane.gesture, Gesture::Panel { .. }) {
            let cur = self.current_chapter();
            if cur != self.pane.panel.follow {
                self.pane.panel.follow = cur;
                if let Some(c) = cur {
                    // Keep the current chapter in view as reading moves on.
                    let row = c as f64 * self.row_h();
                    let visible = (self.pane.vh - self.pane.insets[3] - self.panel_top()) as f64;
                    if row < self.pane.panel.scroll || row + self.row_h() > self.pane.panel.scroll + visible {
                        self.center_chapter(c);
                    }
                }
                self.dirty = true;
            }
        }

        self.shelf_update(now, p);

        let settled = self.pane.anim.is_none() && !matches!(self.pane.gesture, Gesture::Pinch { .. });
        if settled {
            let level = layout::level_for_zoom(self.pane.view.zoom);
            if level != self.pane.render_level {
                self.pane.render_level = level;
                self.pane.requests_dirty = true;
            }
        }
        if self.pane.requests_dirty {
            self.update_requests();
        }
    }

    /// Screen rect of page `p`, snapped to whole pixels when rendering 1:1.
    fn page_rect(&self, l: &Layout, p: usize) -> [f64; 4] {
        let z = self.pane.view.zoom;
        let (mut x0, mut y0) = self.pane.view.to_screen(l.xs[p], l.tops[p]);
        let (mut x1, mut y1) = self.pane.view.to_screen(l.xs[p] + l.widths[p], l.tops[p] + l.heights[p]);
        // Snap both edges so neighbouring pages share exactly the same pixel row.
        if (z - layout::level_scale(self.pane.render_level)).abs() < 1e-9 {
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
        let (cx, cy) = (self.pane.vw as f64 / 2.0, self.pane.vh as f64 / 2.0);
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
        self.pane.requests_dirty = false;
        let Some(d) = &self.pane.doc else { return };
        let id = d.id;
        if self.shelf_visible() {
            // Nothing of the document is on screen.
            self.pane.wanted.clear();
            if !self.pane.sent.is_empty() {
                self.pane.worker.set_queue(id, Vec::new());
                self.pane.sent.clear();
            }
            return;
        }
        let Some(l) = &d.layout else { return };
        let (vw, vh) = (self.pane.vw as f64, self.pane.vh as f64);
        let v = self.pane.view;
        let level = self.pane.render_level;
        let screen = [0.0, 0.0, vw, vh];
        let doc_h = vh / v.zoom;
        let visible = l.range(v.y, v.y + doc_h);
        let cap = self.pane.tiles.budget / 10 * 9;

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
            let complete = vis[start..].iter().all(|(_, r, _)| self.pane.tiles.contains(&r.key));
            // A thumbnail covers the page until its tiles arrive.
            if !complete || self.pane.tiles.contains(&TileKey::thumb(p as u32)) {
                plan(Self::thumb_req(l, p, true), thumb_bytes(p), true);
            }
        }
        vis.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, r, b) in vis {
            plan(r, b, true);
        }

        // Prefetch: a screen ahead in the scroll direction, half a screen behind.
        let ahead = if self.pane.scroll_dir >= 0.0 { [0.0, vh, vw, vh] } else { [0.0, -vh, vw, vh] };
        let behind = if self.pane.scroll_dir >= 0.0 { [0.0, -vh * 0.5, vw, vh * 0.5] } else { [0.0, vh, vw, vh * 0.5] };
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
            order.into_iter().filter(|r| !self.pane.tiles.contains(&r.key) && !self.pane.in_flight.contains(&r.key)).take(160).collect();
        self.pane.wanted = wanted;
        if reqs != self.pane.sent {
            self.pane.worker.set_queue(id, reqs.clone());
            self.pane.sent = reqs;
        }
    }

    // ----- rendering --------------------------------------------------------------------

    fn process_uploads(&mut self, gpu: &mut dyn Gpu) {
        let mut count = 0;
        let mut bytes = 0;
        while let Some(u) = self.pane.uploads.pop_front() {
            self.pane.in_flight.remove(&u.key);
            let dims = match &self.pane.doc {
                Some(d) if d.id == u.doc && self.pane.wanted.contains(&u.key) => d.layout.as_ref().and_then(|l| {
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
                self.pane.tiles.insert(gpu, u.key, u.w, u.h, tw, th, &u.pixels, self.frame);
                count += 1;
                bytes += u.pixels.len();
            }
            self.pane.worker.recycle(u.pixels);
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
        self.drop_retired(Some(gpu));
        if !self.any_shelf() {
            self.covers.clear_gpu(gpu);
        }
        gpu.begin(self.win_w as u32, self.win_h as u32, ui::BG);
        self.with_other(|a| a.render_pane(gpu));
        self.render_pane(gpu);
        gpu.clip(None);
        if self.split() {
            // The divider between the panes.
            let (r, _) = self.slot_rect(0);
            let d = self.divider();
            let mut p = Painter::default();
            if self.win_w >= self.win_h {
                p.rect(r[2], 0.0, d, self.win_h, ui::DIVIDER, 0.0);
            } else {
                p.rect(0.0, r[3], self.win_w, d, ui::DIVIDER, 0.0);
            }
            let atlas = self.text.flush(gpu);
            gpu.draw_ui(&p.v, atlas);
        }
    }

    /// Draws the focused pane (clipped to its rect).
    fn render_pane(&mut self, gpu: &mut dyn Gpu) {
        let pn = &self.pane;
        gpu.clip(Some([pn.x0, pn.y0, pn.w, pn.vh]));
        if self.pane.clear_tiles {
            self.pane.clear_tiles = false;
            self.pane.tiles.clear(gpu);
            for u in std::mem::take(&mut self.pane.uploads) {
                self.pane.worker.recycle(u.pixels);
            }
            self.pane.in_flight.clear();
            self.pane.requests_dirty = true;
        }
        self.process_uploads(gpu);
        if !self.pane.uploads.is_empty() {
            self.dirty = true;
        }
        if self.shelf_visible() {
            return self.render_shelf(gpu);
        }

        let atlas = self.text.flush(gpu);
        let (ox, oy) = (self.pane.x0 + self.pane.ox, self.pane.y0);
        let mut under = Painter { ox, oy, ..Default::default() };
        let mut draws: Vec<(TileKey, [f32; 4])> = Vec::new();
        if let Some(l) = self.layout() {
            let v = self.pane.view;
            let level = self.pane.render_level;
            let screen = [0.0, 0.0, self.pane.vw as f64, self.pane.vh as f64];
            for p in l.range(v.y, v.y + self.pane.vh as f64 / v.zoom) {
                let pr = self.page_rect(l, p);
                under.rect(pr[0] as f32, pr[1] as f32, pr[2] as f32, pr[3] as f32, ui::PLACEHOLDER, 0.0);
                let mut cur = Vec::new();
                self.tiles_in(l, p, level, screen, true, &mut cur);
                let complete = cur.iter().all(|(_, r, _)| self.pane.tiles.contains(&r.key));
                if !complete {
                    let mut others: Vec<TileKey> = self.pane.tiles.page(p as u32).map(|(k, _)| *k).filter(|k| k.level != level).collect();
                    others.sort_by_key(|k| if k.is_thumb() { i32::MIN } else { -(k.level as i32 - level as i32).abs() });
                    for k in others {
                        let r = self.tile_rect(l, &k, pr);
                        if r[0] < screen[2] && r[1] < screen[3] && r[0] + r[2] > 0.0 && r[1] + r[3] > 0.0 {
                            draws.push((k, r.map(|c| c as f32)));
                        }
                    }
                }
                for (_, r, _) in cur {
                    if self.pane.tiles.contains(&r.key) {
                        draws.push((r.key, self.tile_rect(l, &r.key, pr).map(|c| c as f32)));
                    }
                }
            }
        }
        gpu.draw_ui(&under.v, atlas);
        for (k, r) in &draws {
            self.pane.tiles.touch(k, self.frame);
            if let Some(t) = self.pane.tiles.get(k) {
                gpu.draw_texture(t.tex, [r[0] + ox, r[1] + oy, r[2], r[3]], t.uv(), t.clamp());
            }
        }

        let mut over = Painter { ox, oy, ..Default::default() };
        self.draw_highlights(&mut over);
        self.draw_overlay(&mut over);
        let atlas = self.text.flush(gpu);
        gpu.draw_ui(&over.v, atlas);

        self.pane.tiles.evict(gpu, self.frame, &self.pane.wanted);
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
        if self.pane.search.hits.is_empty() {
            return;
        }
        let Some(l) = self.layout() else { return };
        let v = self.pane.view;
        let range = l.range(v.y, v.y + self.pane.vh as f64 / v.zoom);
        let start = self.pane.search.hits.partition_point(|m| (m.page as usize) < range.start);
        let pad = self.dp(1.5);
        for (i, m) in self.pane.search.hits.iter().enumerate().skip(start) {
            if m.page as usize >= range.end {
                break;
            }
            let pr = self.page_rect(l, m.page as usize);
            let color = if Some(i) == self.pane.search.current { ui::HIT_CURRENT } else { ui::HIT };
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
        if matches!(self.pane.gesture, Gesture::Button { hit: h, inside: true, .. } if h == hit) {
            p.circle(x + b / 2.0, cy, b / 2.0 - self.dp(2.0), fade(ui::PRESS, a));
        }
        let on = match hit {
            Hit::Chapters => self.docked(),
            Hit::Split => self.split(),
            Hit::Caffeine => self.caffeine,
            Hit::Settings => self.pane.menu,
            _ => false,
        };
        let color = if on { ui::ACCENT } else { ui::FG };
        ui::icon(p, icon, x + b / 2.0, cy, self.dp(24.0), fade(color, a));
        self.pane.buttons.push(Button { r, hit });
    }

    fn draw_field(&mut self, p: &mut Painter, x: f32, w: f32, cy: f32, placeholder: &str) {
        let h = self.dp(38.0);
        p.rect(x, cy - h / 2.0, w, h, ui::FIELD, h / 2.0);
        self.pane.buttons.push(Button { r: [x, cy - h / 2.0, w, h], hit: Hit::Field });
        let px = self.dp(16.0);
        let pad = self.dp(14.0);
        let inner = w - 2.0 * pad;
        let baseline = cy + self.text.cap_height(px) / 2.0;
        let shown: String =
            if self.pane.mode == Mode::Password { "•".repeat(self.pane.field.chars().count()) } else { self.pane.field.clone() };
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
            let cur = self.pane.cursor.clamp(start, chars.len());
            let before: String = chars[start..cur].iter().collect();
            let cx = x + pad + self.text.measure(&before, px);
            p.rect(cx, cy - self.dp(10.0), self.dp(2.0), self.dp(20.0), ui::ACCENT, 0.0);
        }
    }

    fn draw_overlay(&mut self, p: &mut Painter) {
        self.pane.buttons.clear();
        let a = self.pane.overlay_t;
        let (vw, vh) = (self.pane.vw, self.pane.vh);
        let b = self.dp(48.0);
        if a > 0.01 {
            let top = self.pane.insets[1];
            p.rect(0.0, 0.0, vw, self.bar_bottom(), fade(ui::BAR, a), 0.0);
            if a > 0.5 {
                self.pane.buttons.push(Button { r: [0.0, 0.0, vw, self.bar_bottom()], hit: Hit::Bar });
            }
            let cy = top + self.bar_h() / 2.0;
            let left = (self.pane.insets[0] - self.pane.ox).max(0.0) + self.dp(4.0);
            let right = vw - self.pane.insets[2] - self.dp(4.0);
            match self.pane.mode {
                Mode::Read => {
                    let open = if self.home { Icon::Library } else { Icon::Open };
                    self.icon_button(p, open, left, cy, Hit::Open, a);
                    let mut rx = right - b;
                    self.icon_button(p, Icon::Settings, rx, cy, Hit::Settings, a);
                    let has_outline = self.pane.doc.as_ref().is_some_and(|d| !d.outline.is_empty());
                    if self.pane.doc.is_some() {
                        if has_outline {
                            rx -= b;
                            self.icon_button(p, Icon::Chapters, rx, cy, Hit::Chapters, a);
                        }
                        rx -= b;
                        self.icon_button(p, Icon::Search, rx, cy, Hit::Search, a);
                    }
                    if self.pane.doc.is_some() || self.split() {
                        rx -= b;
                        let icon = if self.win_w >= self.win_h { Icon::Split } else { Icon::SplitRows };
                        self.icon_button(p, icon, rx, cy, Hit::Split, a);
                    }
                    if self.pane.doc.is_some() {
                        rx -= b;
                        self.icon_button(p, Icon::Caffeine, rx, cy, Hit::Caffeine, a);
                        if self.zoomed() {
                            rx -= b;
                            self.icon_button(p, Icon::FitWidth, rx, cy, Hit::FitWidth, a);
                        }
                    }
                    let title = match (&self.pane.doc, &self.pane.pending) {
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
                    let n = self.pane.search.hits.len();
                    let count = match (self.pane.search.current, n) {
                        (Some(c), n) if n > 0 => format!("{}/{}", c + 1, n),
                        _ if self.pane.search.running => String::from("…"),
                        _ if !self.pane.search.query.is_empty() => String::from("0"),
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
                    if self.pane.search.running && self.pane.search.total > 0 {
                        let f = self.pane.search.scanned as f32 / self.pane.search.total as f32;
                        p.rect(0.0, self.bar_bottom() - self.dp(2.0), vw * f, self.dp(2.0), ui::ACCENT, 0.0);
                    }
                }
                Mode::Goto | Mode::Password => {
                    self.icon_button(p, Icon::Back, left, cy, Hit::Close, a);
                    let fx = left + b + self.dp(4.0);
                    let fw = right - fx - self.dp(8.0);
                    let hint = if self.pane.mode == Mode::Goto {
                        format!("Go to page (1–{})", self.pane.doc.as_ref().map_or(0, |d| d.sizes.len()))
                    } else {
                        String::from("Password")
                    };
                    self.draw_field(p, fx, fw, cy, &hint);
                }
            }
        }

        // Page indicator and scrollbar.
        let sa = a.max(self.pane.indicator_t);
        if sa > 0.01 && self.pane.mode != Mode::Password {
            if let Some((n, total, row)) = self.layout().map(|l| (l.len(), l.total, l.row_pages(self.current_page()))) {
                let label =
                    if row.len() > 1 { format!("{}–{} / {}", row.start + 1, row.end, n) } else { format!("{} / {}", row.start + 1, n) };
                let px = self.dp(14.0);
                let w = self.text.measure(&label, px) + self.dp(28.0);
                let h = self.dp(32.0);
                let x = (vw - w) / 2.0;
                let y = vh - self.pane.insets[3] - self.dp(20.0) - h;
                p.rect(x, y, w, h, fade(ui::BAR, sa), h / 2.0);
                let baseline = y + h / 2.0 + self.text.cap_height(px) / 2.0;
                self.text.draw(p, &label, x + self.dp(14.0), baseline, px, fade(ui::FG, sa), w);
                if a > 0.5 {
                    self.pane.buttons.push(Button { r: [x, y - self.dp(8.0), w, h + self.dp(16.0)], hit: Hit::Indicator });
                }
                if self.scrollable() {
                    let (top, bottom) = self.scrollbar_track();
                    let th = self.dp(44.0);
                    let max = (total - vh as f64 / self.pane.view.zoom).max(1.0);
                    let f = (self.pane.view.y / max).clamp(0.0, 1.0) as f32;
                    let ty = top + f * (bottom - top - th);
                    let tw = if matches!(self.pane.gesture, Gesture::Scrollbar) { self.dp(8.0) } else { self.dp(5.0) };
                    let tx = vw - self.pane.insets[2] - self.dp(4.0) - tw;
                    p.rect(tx, ty, tw, th, fade(ui::THUMB, sa), tw / 2.0);
                }
            }
        }

        if self.docked() || self.pane.panel.t > 0.001 {
            // The panel lives in window coordinates, left of the document viewport.
            let ox = std::mem::replace(&mut p.ox, self.pane.x0);
            self.draw_panel(p);
            p.ox = ox;
        }

        if self.pane.doc.is_none() {
            if let Some(pd) = &self.pane.pending {
                if self.now - pd.since > 0.3 {
                    let msg = format!("Opening {}…", pd.name);
                    let px = self.dp(15.0);
                    let w = self.text.measure(&msg, px).min(vw - self.dp(32.0));
                    self.text.draw(p, &msg, (vw - w) / 2.0, vh / 2.0, px, ui::DIM, w + 1.0);
                }
            }
        }

        if self.pane.menu {
            // The menu lives in window coordinates (right edge of the window).
            let ox = std::mem::replace(&mut p.ox, self.pane.x0);
            self.draw_menu(p);
            p.ox = ox;
        }
        self.draw_toast(p, 0.0, vw);
    }

    /// The settings menu card, under the top bar at the right (window coordinates).
    fn menu_rect(&self) -> [f32; 4] {
        let w = self.dp(320.0).min(self.pane.w - self.dp(16.0));
        let x = self.pane.w - self.pane.insets[2] - self.dp(8.0) - w;
        [x.max(self.dp(8.0)), self.bar_bottom() + self.dp(4.0), w, self.dp(72.0) * MENU_ROWS.len() as f32]
    }

    /// The menu row under (`x`, `y`): its rect and what it toggles.
    fn menu_row(&self, x: f32, y: f32) -> Option<([f32; 4], Hit)> {
        let r = self.menu_rect();
        if !inside(r, x, y) {
            return None;
        }
        let h = self.dp(72.0);
        let i = (((y - r[1]) / h) as usize).min(MENU_ROWS.len() - 1);
        Some(([r[0], r[1] + i as f32 * h, r[2], h], MENU_ROWS[i]))
    }

    fn draw_menu(&mut self, p: &mut Painter) {
        let r = self.menu_rect();
        let radius = self.dp(12.0);
        p.rect(r[0] - 1.0, r[1] - 1.0, r[2] + 2.0, r[3] + 2.0, [0, 0, 0, 90], radius + 1.0);
        p.rect(r[0], r[1], r[2], r[3], [44, 44, 50, 255], radius);
        let h = self.dp(72.0);
        for (i, &hit) in MENU_ROWS.iter().enumerate() {
            let y = r[1] + i as f32 * h;
            if matches!(self.pane.gesture, Gesture::Button { hit: g, inside: true, .. } if g == hit) {
                p.rect(r[0], y, r[2], h, ui::PRESS, radius);
            }
            if i > 0 {
                p.rect(r[0] + self.dp(16.0), y, r[2] - self.dp(32.0), 1.0, [255, 255, 255, 18], 0.0);
            }
            let (on, title, sub) = match hit {
                Hit::MenuSpread => (
                    self.spread,
                    "Two pages side by side",
                    if self.spread { "Pages in pairs, like an open book" } else { "Off: one page per row" },
                ),
                _ => (
                    self.home,
                    "Library at start",
                    if self.home { "Book covers when the app opens" } else { "Off: the file picker opens, as before" },
                ),
            };
            // Switch at the right.
            let (sw, sh) = (self.dp(40.0), self.dp(22.0));
            let sx = r[0] + r[2] - self.dp(16.0) - sw;
            let cy = y + h / 2.0;
            p.rect(sx, cy - sh / 2.0, sw, sh, if on { ui::ACCENT } else { [90, 90, 98, 255] }, sh / 2.0);
            let kx = if on { sx + sw - sh / 2.0 } else { sx + sh / 2.0 };
            p.circle(kx, cy, sh / 2.0 - self.dp(3.0), if on { [16, 24, 40, 255] } else { ui::FG });
            let (tpx, spx) = (self.dp(15.0), self.dp(12.5));
            let tx = r[0] + self.dp(16.0);
            let max = (sx - self.dp(12.0) - tx).max(0.0);
            self.text.draw(p, title, tx, cy - self.dp(4.0), tpx, ui::FG, max);
            self.text.draw(p, sub, tx, cy + self.dp(16.0), spx, ui::DIM, max);
        }
    }

    /// Touches while the settings menu is open: its rows toggle, anything else closes it.
    fn menu_touch(&mut self, action: i32, index: usize, p: &mut dyn Platform) {
        let (x, y) = self.pointers.get(index).map_or((0.0, 0.0), |&(_, x, y)| (x, y));
        match action {
            ACTION_DOWN => {
                self.pane.gesture = match self.menu_row(x, y) {
                    Some((r, hit)) => Gesture::Button { hit, r, inside: true },
                    None => {
                        // Closing the menu takes the whole gesture.
                        self.pane.menu = false;
                        Gesture::None
                    }
                };
            }
            ACTION_MOVE => {
                if let (Gesture::Button { hit, r, .. }, Some(&(_, cx, cy))) = (self.pane.gesture, self.pointers.first()) {
                    self.pane.gesture = Gesture::Button { hit, r, inside: inside(r, cx, cy) };
                }
            }
            ACTION_UP => {
                if let Gesture::Button { hit, r, .. } = std::mem::replace(&mut self.pane.gesture, Gesture::None) {
                    if inside(r, x, y) {
                        self.press(hit, p);
                    }
                }
            }
            _ => self.pane.gesture = Gesture::None,
        }
    }

    /// The toast message, centred in the `w` wide area starting at `x0`.
    fn draw_toast(&mut self, p: &mut Painter, x0: f32, w: f32) {
        let Some((msg, _)) = &self.pane.toast else { return };
        let msg = msg.clone();
        let px = self.dp(14.0);
        let tw = (self.text.measure(&msg, px) + self.dp(32.0)).min(w - self.dp(32.0));
        let h = self.dp(40.0);
        let x = x0 + (w - tw) / 2.0;
        let y = self.pane.vh - self.pane.insets[3] - self.dp(72.0) - h;
        p.rect(x, y, tw, h, [48, 48, 54, 245], h / 2.0);
        let baseline = y + h / 2.0 + self.text.cap_height(px) / 2.0;
        self.text.draw(p, &msg, x + self.dp(16.0), baseline, px, ui::FG, tw - self.dp(32.0));
    }

    fn draw_panel(&mut self, p: &mut Painter) {
        let docked = self.docked();
        let t = if docked { 1.0 } else { ease_out(self.pane.panel.t as f64) as f32 };
        let vh = self.pane.vh;
        let pw = self.panel_w();
        let x0 = -pw * (1.0 - t);
        if !docked {
            p.rect(0.0, 0.0, self.pane.w, vh, [0, 0, 0, (130.0 * t) as u8], 0.0);
        }
        p.rect(x0, 0.0, pw, vh, ui::PANEL, 0.0);
        let resizing = matches!(self.pane.gesture, Gesture::PanelResize { .. });
        if docked {
            // Shrinking preview: blank the strip the document will move into.
            p.rect(pw, 0.0, self.pane.ox - pw, vh, [18, 18, 20, 255], 0.0);
            p.rect(pw - 1.0, 0.0, 1.0, vh, [255, 255, 255, 18], 0.0);
        }
        let Some(d) = &self.pane.doc else { return };
        let items = d.outline.clone();
        let n_pages = d.sizes.len();
        let current = self.current_chapter();
        let top = self.panel_top();
        let row_h = self.row_h();
        let bottom = vh - self.pane.insets[3];
        let first = (self.pane.panel.scroll / row_h).floor().max(0.0) as usize;
        // Keep text clear of a left display cutout / navigation bar.
        let il = self.pane.insets[0];
        let tpx = self.dp(15.0);
        let npx = self.dp(13.0);
        for (i, it) in items.iter().enumerate().skip(first) {
            let y = top + (i as f64 * row_h - self.pane.panel.scroll) as f32;
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
        let baseline = self.pane.insets[1] + self.dp(28.0) + self.text.cap_height(hpx) / 2.0;
        let pin = self.pin_rect();
        self.text.draw(p, "Contents", x0 + il + self.dp(20.0), baseline, hpx, ui::FG, (pin[0] - il - self.dp(24.0)).max(0.0));
        let (bx, cy) = (x0 + pin[0] + pin[2] / 2.0, pin[1] + pin[3] / 2.0);
        if matches!(self.pane.gesture, Gesture::Panel { x, start, moved: false, .. } if inside(pin, x, start)) {
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
