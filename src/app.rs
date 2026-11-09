//! Application state machine: input, scrolling, tiles, search, overlay.
//! Platform independent; the Android glue feeds it events and a GL backend.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use crate::gesture::{Fling, VelocityTracker};
use crate::gfx::{Gpu, Painter};
use crate::layout::{self, Layout, View, MAX_ZOOM, MIN_ZOOM};
use crate::pdfium::OutlineItem;
use crate::search;
use crate::store::{Position, Store};
use crate::text::Text;
use crate::tiles::{TileCache, TileKey};
use crate::ui::{self, fade, Icon};
use crate::worker::{Msg, RenderReq, Sink, Worker};

/// Kind of soft keyboard to show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keyboard {
    Text,
    Number,
    Password,
}

/// Something only the host platform can do. The app queues these while handling
/// events and updating; the platform loop runs them via [`App::drain_commands`].
#[derive(Clone, PartialEq, Debug)]
pub enum Command {
    OpenPicker,
    /// Show the soft keyboard for an editable field with this initial text.
    ShowKeyboard(Keyboard, String),
    HideKeyboard,
    SystemBars(bool),
    Exit,
}

/// What happened to the pointer at `index` in a touch event.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TouchPhase {
    /// First finger down.
    Down,
    /// Another finger down.
    PointerDown,
    Move,
    /// A finger up while others stay down.
    PointerUp,
    /// Last finger up.
    Up,
    Cancel,
}

/// Navigation keys (hardware keyboard, remote, etc.).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
}

#[derive(Debug)]
pub enum Event {
    /// Safe-area insets in px: left, top, right, bottom.
    Insets([f32; 4]),
    /// `phase` applies to the pointer at `index`; `pointers` holds (id, x, y) for
    /// every pointer currently down; `time` is the event time in s.
    Touch {
        phase: TouchPhase,
        index: usize,
        pointers: Vec<(i32, f32, f32)>,
        time: f64,
    },
    Wheel {
        x: f32,
        y: f32,
        delta: f32,
    },
    Key(Key),
    Back,
    Open {
        fd: i32,
        uri: String,
        name: String,
    },
    OpenFailed(String),
    PickerCancelled,
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
    Bar,
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
    Pending { x: f32, y: f32, t: f64, stopped: bool },
    Drag { id: i32, x: f32, y: f32 },
    Pinch { d0: f64, z0: f64, doc: (f64, f64) },
    Button { hit: Hit, r: [f32; 4], inside: bool },
    Scrollbar,
    Panel { y: f32, start: f32, moved: bool },
    PanelOutside,
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
    open: bool,
    t: f32,
    scroll: f64,
    fling: Option<Fling>,
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
    vw: f32,
    vh: f32,
    insets: [f32; 4],
    now: f64,
    last_update: f64,
    store: Store,
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
    /// Platform requests queued since the last `drain_commands`.
    commands: Vec<Command>,
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
        let store = Store::load(files_dir);
        let spread = store.spread();
        App {
            density: density.max(0.5),
            vw: 0.0,
            vh: 0.0,
            insets: [0.0; 4],
            now: 0.0,
            last_update: 0.0,
            store,
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
            commands: Vec::new(),
        }
    }

    fn dp(&self, v: f32) -> f32 {
        v * self.density
    }

    fn command(&mut self, c: Command) {
        self.commands.push(c);
    }

    /// Platform requests queued by `handle`/`update`, in order. The platform loop
    /// runs them after each batch of events.
    pub fn drain_commands(&mut self) -> std::vec::Drain<'_, Command> {
        self.commands.drain(..)
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
        self.panel.open || self.mode != Mode::Read
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
        if self.overlay && self.mode == Mode::Read && !self.panel.open {
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
        t
    }

    // ----- geometry ---------------------------------------------------------------------

    fn bar_h(&self) -> f32 {
        self.dp(56.0)
    }

    fn bar_bottom(&self) -> f32 {
        self.insets[1] + self.bar_h()
    }

    fn panel_w(&self) -> f32 {
        self.dp(360.0).min(self.vw * 0.86)
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
        self.overlay || self.mode != Mode::Read || self.doc.is_none()
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
        if w == self.vw && h == self.vh {
            return;
        }
        let width_changed = w != self.vw;
        let pos = self.position();
        self.vw = w;
        self.vh = h;
        let screen = (w * h * 4.0) as usize;
        self.tiles.budget = (screen * 8).clamp(96 << 20, 256 << 20);
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
        let Some(l) = self.layout() else { return };
        let v = self.view;
        let range = l.range(v.y, v.y + self.vh as f64 / v.zoom);
        let level = self.render_level;
        self.tiles.retain(gpu, |k| range.contains(&(k.page as usize)) && (k.level == level || k.is_thumb()));
    }

    /// The GL context was lost; every texture is gone.
    pub fn gpu_lost(&mut self) {
        self.tiles.forget();
        self.text.forget_gpu();
        self.requests_dirty = true;
        self.dirty = true;
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), self.now + 2.5));
        self.dirty = true;
    }

    // ----- events -----------------------------------------------------------------------

    pub fn handle(&mut self, ev: Event, now: f64) {
        self.now = now;
        self.dirty = true;
        match ev {
            Event::Insets(i) => self.insets = i,
            Event::Touch { phase, index, pointers, time } => self.touch(phase, index, pointers, time),
            Event::Wheel { delta, .. } => {
                let dy = -(delta as f64) * self.dp(64.0) as f64 / self.view.zoom;
                self.anim = None;
                self.fling = None;
                self.scroll_dir = if dy >= 0.0 { 1.0 } else { -1.0 };
                self.set_view(View { y: self.view.y + dy, ..self.view });
                self.touch_activity();
            }
            Event::Key(code) => self.key(code),
            Event::Back => {
                self.back();
            }
            Event::Open { fd, uri, name } => {
                self.save_position();
                let id = self.next_id;
                self.next_id += 1;
                self.worker.open(id, fd);
                if self.mode == Mode::Password {
                    self.set_mode(Mode::Read);
                }
                self.pending = Some(Pending { id, uri, name, since: now });
            }
            Event::OpenFailed(msg) => {
                crate::log_w!("open failed: {msg}");
                self.toast("Can't open this file");
                if self.doc.is_none() && self.pending.is_none() {
                    self.command(Command::OpenPicker);
                }
            }
            Event::PickerCancelled => {
                if self.doc.is_none() && self.pending.is_none() {
                    self.command(Command::Exit);
                }
            }
            Event::Text { text, cursor } => {
                self.cursor = utf16_to_char_index(&text, cursor);
                if text != self.field {
                    self.field = text;
                    if self.mode == Mode::Search {
                        self.search.debounce = Some(now + 0.25);
                    }
                }
            }
            Event::ImeAction => self.ime_action(),
            Event::Pause => self.save_position(),
            Event::Worker(msg) => self.worker_msg(msg),
        }
    }

    fn worker_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Opened { doc, sizes } => {
                let Some(pending) = self.pending.take_if(|pd| pd.id == doc) else { return };
                let restore = self.store.get(&pending.uri);
                self.doc = Some(Doc { id: doc, uri: pending.uri, name: pending.name, sizes, outline: Vec::new(), layout: None, restore });
                self.clear_tiles = true;
                self.sent.clear();
                self.wanted.clear();
                self.search = SearchState::default();
                self.panel = Panel::default();
                if self.mode != Mode::Read {
                    self.set_mode(Mode::Read);
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
                    self.command(Command::ShowKeyboard(Keyboard::Password, String::new()));
                    self.keyboard = true;
                    if wrong {
                        self.toast("Wrong password");
                    }
                }
            }
            Msg::OpenFailed { doc, error } => {
                if self.pending.take_if(|pd| pd.id == doc).is_some() {
                    crate::log_w!("cannot open: {error}");
                    self.toast("Can't open this file");
                    if self.doc.is_none() {
                        self.command(Command::OpenPicker);
                    }
                }
            }
            Msg::Outline { doc, items } => {
                if let Some(d) = self.doc.as_mut().filter(|d| d.id == doc) {
                    d.outline = items;
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
        }
    }

    fn key(&mut self, key: Key) {
        let vh = self.vh as f64 / self.view.zoom;
        let vw = self.vw as f64 / self.view.zoom;
        let v = self.view;
        let to = match key {
            Key::Down => View { y: v.y + vh * 0.15, ..v },
            Key::Up => View { y: v.y - vh * 0.15, ..v },
            Key::PageDown => View { y: v.y + vh * 0.85, ..v },
            Key::PageUp => View { y: v.y - vh * 0.85, ..v },
            Key::Home => View { y: 0.0, ..v },
            Key::End => View { y: f64::MAX / 4.0, ..v },
            Key::Left => View { x: v.x - vw * 0.2, ..v },
            Key::Right => View { x: v.x + vw * 0.2, ..v },
        };
        self.scroll_dir = if to.y >= v.y { 1.0 } else { -1.0 };
        self.animate_to(to, 0.18);
        self.touch_activity();
    }

    fn back(&mut self) -> bool {
        if self.panel.open {
            self.panel.open = false;
            return true;
        }
        if self.mode != Mode::Read {
            self.set_mode(Mode::Read);
            return true;
        }
        false
    }

    fn set_mode(&mut self, mode: Mode) {
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
            if self.doc.is_none() {
                self.command(Command::OpenPicker);
            }
        }
        self.mode = mode;
        self.panel.open = false;
        match mode {
            Mode::Read => {
                if self.keyboard {
                    self.command(Command::HideKeyboard);
                    self.keyboard = false;
                }
                self.overlay = true;
                self.overlay_until = self.now + OVERLAY_TIMEOUT;
            }
            Mode::Search => {
                self.field = self.search.text.clone();
                self.cursor = self.field.chars().count();
                self.command(Command::ShowKeyboard(Keyboard::Text, self.field.clone()));
                self.keyboard = true;
                if !self.field.is_empty() {
                    self.search.query.clear();
                    self.start_search();
                }
            }
            Mode::Goto => {
                self.field.clear();
                self.cursor = 0;
                self.command(Command::ShowKeyboard(Keyboard::Number, String::new()));
                self.keyboard = true;
            }
            Mode::Password => {}
        }
    }

    fn ime_action(&mut self) {
        match self.mode {
            Mode::Search => {
                if self.search.debounce.is_some() || search::fold_query(&self.field) != self.search.query {
                    self.start_search();
                } else {
                    self.step_match(1);
                }
                self.command(Command::HideKeyboard);
                self.keyboard = false;
            }
            Mode::Goto => {
                let n = self.doc.as_ref().map_or(0, |d| d.sizes.len());
                match self.field.trim().parse::<usize>() {
                    Ok(page) if page >= 1 && page <= n => {
                        self.jump_to(page - 1, 0.0);
                        self.set_mode(Mode::Read);
                        self.overlay = false;
                    }
                    _ => self.toast(format!("Enter a page from 1 to {n}")),
                }
            }
            Mode::Password => {
                if let Some(pd) = &self.pending {
                    self.worker.password(pd.id, self.field.clone());
                    self.command(Command::HideKeyboard);
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

    fn touch(&mut self, phase: TouchPhase, index: usize, pointers: Vec<(i32, f32, f32)>, time: f64) {
        self.pointers = pointers;
        let (px, py) = self.pointers.get(index).map_or((0.0, 0.0), |&(_, x, y)| (x, y));
        match phase {
            TouchPhase::Down => self.on_down(px, py, time),
            TouchPhase::PointerDown => self.on_pointer_down(),
            TouchPhase::Move => self.on_move(time),
            TouchPhase::PointerUp => self.on_pointer_up(index),
            TouchPhase::Up => self.on_up(px, py, time),
            TouchPhase::Cancel => {
                self.gesture = Gesture::None;
                self.vel.reset();
            }
        }
    }

    fn on_down(&mut self, x: f32, y: f32, time: f64) {
        self.vel.reset();
        self.vel.add(time, x as f64, y as f64);
        if self.panel.open {
            self.panel.fling = None;
            self.gesture = if x < self.panel_w() { Gesture::Panel { y, start: y, moved: false } } else { Gesture::PanelOutside };
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
            Gesture::Panel { y, start, moved } => {
                let Some(&(_, _, cy)) = self.pointers.first() else { return };
                self.vel.add(time, 0.0, cy as f64);
                let moved = moved || (cy - start).abs() > slop;
                if moved {
                    let max = self.panel_max_scroll();
                    self.panel.scroll = (self.panel.scroll - (cy - y) as f64).clamp(0.0, max);
                }
                self.gesture = Gesture::Panel { y: cy, start, moved };
            }
            Gesture::PanelOutside | Gesture::None => {}
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

    fn on_up(&mut self, x: f32, y: f32, time: f64) {
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
                    self.press(hit);
                }
            }
            Gesture::Scrollbar => {}
            Gesture::Panel { moved, .. } => {
                if moved {
                    let (_, vy) = self.vel.velocity(time);
                    self.panel.fling = Fling::new(self.now, 0.0, vy, self.density as f64);
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
            Gesture::None => {}
        }
    }

    fn single_tap(&mut self, _x: f32, _y: f32) {
        if self.keyboard && self.mode != Mode::Read {
            self.command(Command::HideKeyboard);
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

    fn press(&mut self, hit: Hit) {
        self.overlay_until = self.now + OVERLAY_TIMEOUT;
        match hit {
            Hit::Open => self.command(Command::OpenPicker),
            Hit::Search => self.set_mode(Mode::Search),
            Hit::Chapters => self.open_panel(),
            Hit::Spread => self.toggle_spread(),
            Hit::Close => self.set_mode(Mode::Read),
            Hit::Prev => self.step_match(-1),
            Hit::Next => self.step_match(1),
            Hit::Field => {
                let kb = match self.mode {
                    Mode::Goto => Keyboard::Number,
                    Mode::Password => Keyboard::Password,
                    _ => Keyboard::Text,
                };
                self.command(Command::ShowKeyboard(kb, self.field.clone()));
                self.keyboard = true;
            }
            Hit::Indicator => {
                if self.mode == Mode::Read {
                    self.set_mode(Mode::Goto);
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
        let visible = (self.vh - self.insets[3] - self.panel_top()) as f64;
        self.panel.scroll = (cur as f64 * self.row_h() - visible / 2.0 + self.row_h() / 2.0).clamp(0.0, self.panel_max_scroll());
        self.panel.open = true;
        self.panel.fling = None;
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

    pub fn update(&mut self, now: f64) {
        let dt = (now - self.last_update).clamp(0.0, 0.1) as f32;
        self.last_update = now;
        self.now = now;

        if let Some((at, x, y)) = self.tap_at {
            if now >= at {
                self.tap_at = None;
                self.single_tap(x, y);
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

        if self.overlay && self.mode == Mode::Read && !self.panel.open && now >= self.overlay_until {
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

        let bars = self.overlay_visible();
        if self.bars != Some(bars) {
            self.bars = Some(bars);
            self.command(Command::SystemBars(bars));
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
        let Some(l) = &d.layout else { return };
        let id = d.id;
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

        gpu.begin(self.vw as u32, self.vh as u32, ui::BG);
        let atlas = self.text.flush(gpu);
        let mut under = Painter::default();
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
                gpu.draw_texture(t.tex, *r, t.uv(), t.clamp());
            }
        }

        let mut over = Painter::default();
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
        let color = if hit == Hit::Spread && self.spread { ui::ACCENT } else { ui::FG };
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
            let left = self.insets[0] + self.dp(4.0);
            let right = vw - self.insets[2] - self.dp(4.0);
            match self.mode {
                Mode::Read => {
                    self.icon_button(p, Icon::Open, left, cy, Hit::Open, a);
                    let mut rx = right;
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

        if self.panel.t > 0.001 {
            self.draw_panel(p);
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

        if let Some((msg, _)) = self.toast.clone() {
            let px = self.dp(14.0);
            let w = (self.text.measure(&msg, px) + self.dp(32.0)).min(vw - self.dp(32.0));
            let h = self.dp(40.0);
            let x = (vw - w) / 2.0;
            let y = vh - self.insets[3] - self.dp(72.0) - h;
            p.rect(x, y, w, h, [48, 48, 54, 245], h / 2.0);
            let baseline = y + h / 2.0 + self.text.cap_height(px) / 2.0;
            self.text.draw(p, &msg, x + self.dp(16.0), baseline, px, ui::FG, w - self.dp(32.0));
        }
    }

    fn draw_panel(&mut self, p: &mut Painter) {
        let t = ease_out(self.panel.t as f64) as f32;
        let (vw, vh) = (self.vw, self.vh);
        let pw = self.panel_w();
        let x0 = -pw * (1.0 - t);
        p.rect(0.0, 0.0, vw, vh, [0, 0, 0, (130.0 * t) as u8], 0.0);
        p.rect(x0, 0.0, pw, vh, ui::PANEL, 0.0);
        let Some(d) = &self.doc else { return };
        let items = d.outline.clone();
        let n_pages = d.sizes.len();
        let current = self.current_chapter();
        let top = self.panel_top();
        let row_h = self.row_h();
        let bottom = vh - self.insets[3];
        let first = (self.panel.scroll / row_h).floor().max(0.0) as usize;
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
            let indent = self.dp(20.0) + self.dp(14.0) * it.depth.min(6) as f32;
            let tx = x0 + indent;
            let color = if it.depth == 0 { ui::FG } else { [205, 205, 212, 255] };
            let title = if it.title.is_empty() { "—" } else { it.title.as_str() };
            self.text.draw(p, title, tx, base + self.text.cap_height(tpx) / 2.0, tpx, color, (nx - self.dp(12.0) - tx).max(0.0));
        }
        // Header drawn last so rows scroll underneath it.
        p.rect(x0, 0.0, pw, top, ui::PANEL, 0.0);
        let hpx = self.dp(18.0);
        let baseline = self.insets[1] + self.dp(28.0) + self.text.cap_height(hpx) / 2.0;
        self.text.draw(p, "Contents", x0 + self.dp(20.0), baseline, hpx, ui::FG, pw - self.dp(40.0));
        p.rect(x0, top - 1.0, pw, 1.0, [255, 255, 255, 18], 0.0);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(2.0, None, Box::new(|_| {}))
    }

    #[test]
    fn picker_cancelled_without_document_exits() {
        let mut a = app();
        a.handle(Event::PickerCancelled, 0.0);
        assert_eq!(a.drain_commands().collect::<Vec<_>>(), [Command::Exit]);
        assert_eq!(a.drain_commands().count(), 0);
    }

    #[test]
    fn system_bars_sent_once_per_change() {
        let mut a = app();
        a.update(0.0);
        assert_eq!(a.drain_commands().collect::<Vec<_>>(), [Command::SystemBars(true)]);
        a.update(0.1);
        assert_eq!(a.drain_commands().count(), 0);
    }
}
