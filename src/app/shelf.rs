//! The library screen ("shelf"): a grid of book covers, either the recently
//! opened documents or the PDFs found in a folder the user added (grouped by
//! subfolder). Shown at launch and from the reader's Open button.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Range;
use std::path::PathBuf;

use super::*;
use crate::covers;
use crate::gfx::TexId;
use crate::library::{self, Book};
use crate::worker::CoverSrc;

/// Rescan the library folders when the shelf is shown and the last scan is older.
const RESCAN_AFTER: f64 = 30.0;
/// Cover jobs (disk loads, fds being opened, renders) in flight at once.
const COVER_JOBS: usize = 4;
/// Cover textures kept on the GPU before the least recently drawn are dropped.
const COVER_TEXTURES: usize = 150;

pub(super) struct ShelfItem {
    uri: String,
    title: String,
    key: u64,
    /// (current page, page count), 1-based, if the document was read before.
    progress: Option<(u32, u32)>,
}

enum RowKind {
    /// Subfolder heading: path, number of books.
    Header(String, usize),
    /// One line of the grid: indices into `Shelf::items`.
    Cells(Range<usize>),
}

pub(super) struct Row {
    /// Top relative to the start of the scrolled content.
    y: f64,
    h: f64,
    kind: RowKind,
}

pub(super) struct Shelf {
    /// Shown over an open document (it is always shown without one).
    pub open: bool,
    /// 0 = recent documents, i + 1 = library folder i.
    pub tab: usize,
    pub scroll: f64,
    pub fling: Option<Fling>,
    tabs_scroll: f32,
    tabs_max: f32,
    /// Tab chips as last drawn (window coordinates).
    chips: Vec<[f32; 4]>,
    items: Vec<ShelfItem>,
    rows: Vec<Row>,
    height: f64,
    /// Contents must be rebuilt (store/library changed).
    pub stale: bool,
    /// Window width and side insets the rows were built for.
    built: [f32; 3],
    /// A book being opened from the shelf: title, since.
    pub opening: Option<(String, f64)>,
    /// Folders whose scan is running.
    scanning: HashSet<String>,
    last_scan: Option<f64>,
    /// "Remove folder" was tapped once; a second tap before this time removes it.
    remove_armed: Option<f64>,
}

impl Shelf {
    pub fn new(lib: &Library, tab_uri: &str) -> Shelf {
        let tab = if tab_uri.is_empty() { 0 } else { lib.find(tab_uri).map_or(0, |i| i + 1) };
        Shelf {
            open: false,
            tab,
            scroll: 0.0,
            fling: None,
            tabs_scroll: 0.0,
            tabs_max: 0.0,
            chips: Vec::new(),
            items: Vec::new(),
            rows: Vec::new(),
            height: 0.0,
            stale: true,
            built: [0.0; 3],
            opening: None,
            scanning: HashSet::new(),
            last_scan: None,
            remove_armed: None,
        }
    }
}

enum Slot {
    /// Asked for (or waiting to be uploaded).
    Pending,
    Failed,
    Ready {
        tex: TexId,
        w: u32,
        h: u32,
        used: u64,
    },
}

pub(super) struct Covers {
    /// Disk cache directory (no covers without one).
    dir: Option<PathBuf>,
    /// Keys cached on disk.
    disk: HashSet<u64>,
    slots: HashMap<u64, Slot>,
    /// Keys with a job in flight for the shelf.
    busy: HashSet<u64>,
    uploads: VecDeque<(u64, u32, u32, Vec<u8>)>,
    /// Number of `Slot::Ready` (textures on the GPU).
    textures: usize,
}

impl Covers {
    pub fn new(dir: Option<PathBuf>) -> Covers {
        let disk = dir.as_deref().map(covers::index).unwrap_or_default();
        Covers { dir, disk, slots: HashMap::new(), busy: HashSet::new(), uploads: VecDeque::new(), textures: 0 }
    }

    pub fn uploading(&self) -> bool {
        !self.uploads.is_empty()
    }

    /// The GL context is gone (and every texture with it).
    pub fn forget_gpu(&mut self) {
        self.slots.retain(|_, s| !matches!(s, Slot::Ready { .. }));
        self.textures = 0;
    }

    /// Frees every cover texture (the shelf is not shown).
    pub fn clear_gpu(&mut self, gpu: &mut dyn Gpu) {
        for (key, ..) in self.uploads.drain(..) {
            self.slots.remove(&key);
        }
        if self.textures == 0 {
            return;
        }
        self.textures = 0;
        self.slots.retain(|_, s| match s {
            Slot::Ready { tex, .. } => {
                gpu.delete_texture(*tex);
                false
            }
            _ => true,
        });
    }

    /// Uploads a few finished covers; true if more are waiting.
    fn upload(&mut self, gpu: &mut dyn Gpu, frame: u64) -> bool {
        let mut n = 0;
        while n < 6 {
            let Some((key, w, h, px)) = self.uploads.pop_front() else { break };
            if !matches!(self.slots.get(&key), Some(Slot::Pending)) {
                continue;
            }
            let tex = gpu.create_texture(w, h, false);
            gpu.upload(tex, 0, 0, w, h, &px, false);
            self.slots.insert(key, Slot::Ready { tex, w, h, used: frame });
            self.textures += 1;
            n += 1;
        }
        !self.uploads.is_empty()
    }

    /// Drops the least recently drawn textures beyond the limit.
    fn evict(&mut self, gpu: &mut dyn Gpu, frame: u64) {
        if self.textures <= COVER_TEXTURES {
            return;
        }
        let mut ready: Vec<(u64, u64)> =
            self.slots.iter().filter_map(|(k, s)| if let Slot::Ready { used, .. } = s { Some((*used, *k)) } else { None }).collect();
        if ready.len() <= COVER_TEXTURES {
            return;
        }
        ready.sort_unstable();
        let excess = ready.len() - COVER_TEXTURES * 4 / 5;
        for (used, key) in ready.into_iter().take(excess) {
            if used >= frame {
                break;
            }
            if let Some(Slot::Ready { tex, .. }) = self.slots.remove(&key) {
                gpu.delete_texture(tex);
                self.textures -= 1;
            }
        }
    }
}

/// A readable name for a URI without a known display name.
fn name_from_uri(uri: &str) -> String {
    let mut bytes = Vec::with_capacity(uri.len());
    let b = uri.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                bytes.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                bytes.push(c);
                i += 1;
            }
        }
    }
    let s = String::from_utf8_lossy(&bytes);
    let tail = s.rsplit(['/', ':']).find(|t| !t.is_empty()).unwrap_or("Document");
    tail.to_string()
}

struct Grid {
    left: f32,
    cols: usize,
    cell_w: f32,
    gap: f32,
    cover_h: f32,
    cell_h: f32,
}

impl App {
    // ----- geometry ---------------------------------------------------------------------

    /// Top of the tab strip.
    fn shelf_tabs_top(&self) -> f32 {
        self.insets[1] + self.dp(56.0)
    }

    /// Top of the scrolled grid.
    fn shelf_top(&self) -> f32 {
        self.shelf_tabs_top() + self.dp(52.0)
    }

    fn shelf_area(&self) -> f64 {
        (self.vh - self.shelf_top()).max(1.0) as f64
    }

    fn shelf_max_scroll(&self) -> f64 {
        (self.shelf.height + (self.insets[3] + self.dp(16.0)) as f64 - self.shelf_area()).max(0.0)
    }

    fn shelf_grid(&self) -> Grid {
        let left = self.insets[0] + self.dp(16.0);
        let width = (self.win_w - self.insets[2] - self.dp(16.0) - left).max(self.dp(100.0));
        let gap = self.dp(12.0);
        let cols = (((width + gap) / (self.dp(104.0) + gap)) as usize).clamp(2, 12);
        let cell_w = (width - gap * (cols - 1) as f32) / cols as f32;
        let cover_h = (cell_w * 1.4).round();
        Grid { left, cols, cell_w, gap, cover_h, cell_h: cover_h + self.dp(48.0) }
    }

    // ----- contents ---------------------------------------------------------------------

    /// Rebuilds the items and rows of the current tab.
    fn shelf_build(&mut self) {
        let g = self.shelf_grid();
        self.shelf.stale = false;
        self.shelf.built = [self.win_w, self.insets[0], self.insets[2]];
        if self.shelf.tab > self.library.folders.len() {
            self.shelf.tab = 0;
        }
        let progress = |e: &crate::store::Entry| (e.pages > 0).then(|| ((e.pos.page + 1).min(e.pages), e.pages));
        let mut items = Vec::new();
        // (heading, item range) groups.
        let mut groups: Vec<(Option<String>, Range<usize>)> = Vec::new();
        if self.shelf.tab == 0 {
            for e in self.store.recent() {
                let name = if e.name.is_empty() { name_from_uri(&e.uri) } else { e.name.clone() };
                let title = library::title(&name).to_string();
                items.push(ShelfItem { uri: e.uri.clone(), title, key: covers::key(&e.uri), progress: progress(e) });
            }
            groups.push((None, 0..items.len()));
        } else {
            let f = &self.library.folders[self.shelf.tab - 1];
            let known: HashMap<&str, (u32, u32)> =
                self.store.recent().iter().filter_map(|e| Some((e.uri.as_str(), progress(e)?))).collect();
            let sections = library::sections(&f.books);
            let headings = sections.len() > 1 || sections.first().is_some_and(|(d, _)| !d.is_empty());
            for (dir, range) in sections {
                let heading = headings.then(|| if dir.is_empty() { f.name.clone() } else { dir.replace('/', " / ") });
                groups.push((heading, range.clone()));
                for Book { uri, name, .. } in &f.books[range] {
                    let title = library::title(name).to_string();
                    let progress = known.get(uri.as_str()).copied();
                    items.push(ShelfItem { uri: uri.clone(), title, key: covers::key(uri), progress });
                }
            }
        }
        let mut rows = Vec::new();
        let mut y = self.dp(8.0) as f64;
        let header_h = self.dp(44.0) as f64;
        let row_h = (g.cell_h + self.dp(12.0)) as f64;
        for (heading, range) in groups {
            if let Some(h) = heading {
                rows.push(Row { y, h: header_h, kind: RowKind::Header(h, range.len()) });
                y += header_h;
            }
            let mut i = range.start;
            while i < range.end {
                let end = (i + g.cols).min(range.end);
                rows.push(Row { y, h: row_h, kind: RowKind::Cells(i..end) });
                y += row_h;
                i = end;
            }
        }
        self.shelf.items = items;
        self.shelf.rows = rows;
        self.shelf.height = y;
        self.shelf.scroll = self.shelf.scroll.clamp(0.0, self.shelf_max_scroll());
        self.dirty = true;
    }

    fn shelf_item_at(&self, x: f32, y: f32) -> Option<usize> {
        let g = self.shelf_grid();
        let cy = (y - self.shelf_top()) as f64 + self.shelf.scroll;
        if y < self.shelf_top() {
            return None;
        }
        let row = self.shelf.rows.iter().find(|r| cy >= r.y && cy < r.y + r.h)?;
        let RowKind::Cells(range) = &row.kind else { return None };
        if cy > row.y + g.cell_h as f64 || x < g.left {
            return None;
        }
        let col = ((x - g.left) / (g.cell_w + g.gap)) as usize;
        let in_cell = x - g.left - col as f32 * (g.cell_w + g.gap) <= g.cell_w;
        let i = range.start + col;
        (in_cell && i < range.end).then_some(i)
    }

    // ----- actions ----------------------------------------------------------------------

    /// Shows the library over the open document.
    pub(super) fn show_shelf(&mut self, p: &mut dyn Platform) {
        self.save_position();
        if self.mode != Mode::Read {
            self.set_mode(Mode::Read, p);
        }
        self.panel.open = false;
        self.shelf.open = true;
        self.shelf.stale = true;
        self.shelf.scroll = 0.0;
        self.shelf.fling = None;
        self.gesture = Gesture::None;
        self.requests_dirty = true;
        if self.shelf.last_scan.is_none_or(|t| self.now - t > RESCAN_AFTER) {
            self.scan_all(p);
        }
    }

    /// Back to the open document.
    pub(super) fn hide_shelf(&mut self) {
        self.shelf.open = false;
        self.shelf.fling = None;
        self.shelf.remove_armed = None;
        self.gesture = Gesture::None;
        self.invalidate();
        self.overlay = true;
        self.overlay_until = self.now + OVERLAY_TIMEOUT;
    }

    fn scan_all(&mut self, p: &mut dyn Platform) {
        self.shelf.last_scan = Some(self.now);
        for f in &self.library.folders {
            if self.shelf.scanning.insert(f.uri.clone()) {
                p.scan_folder(&f.uri);
            }
        }
        self.dirty = true;
    }

    fn select_tab(&mut self, tab: usize) {
        if tab > self.library.folders.len() {
            return;
        }
        if tab != self.shelf.tab {
            self.shelf.tab = tab;
            self.shelf.scroll = 0.0;
        }
        self.shelf.fling = None;
        self.shelf.stale = true;
        self.shelf.remove_armed = None;
        let uri = if tab == 0 { String::new() } else { self.library.folders[tab - 1].uri.clone() };
        self.store.set_shelf(&uri);
    }

    fn open_book(&mut self, i: usize, p: &mut dyn Platform) {
        let Some(item) = self.shelf.items.get(i) else { return };
        if self.doc.as_ref().is_some_and(|d| d.uri == item.uri) {
            return self.hide_shelf();
        }
        self.shelf.opening = Some((item.title.clone(), self.now));
        let uri = item.uri.clone();
        p.open_document(&uri);
    }

    pub(super) fn shelf_press(&mut self, hit: Hit, p: &mut dyn Platform) {
        match hit {
            Hit::ShelfBack => self.hide_shelf(),
            Hit::ShelfOpenFile => p.open_picker(),
            Hit::ShelfAddFolder => p.open_folder_picker(),
            Hit::ShelfRemove => {
                let Some(f) = self.shelf.tab.checked_sub(1).and_then(|i| self.library.folders.get(i)) else { return };
                let name = f.name.clone();
                if self.shelf.remove_armed.is_some_and(|t| self.now < t) {
                    if let Some(f) = self.library.remove(self.shelf.tab - 1) {
                        self.shelf.scanning.remove(&f.uri);
                        p.release_folder(&f.uri);
                    }
                    self.select_tab(0);
                    self.shelf.tabs_scroll = 0.0;
                    self.toast(format!("“{name}” removed from the library"));
                } else {
                    self.shelf.remove_armed = Some(self.now + 3.0);
                    self.toast(format!("Tap again to remove “{name}” (files stay)"));
                }
            }
            _ => {}
        }
    }

    pub(super) fn folder_picked(&mut self, uri: &str, name: &str, p: &mut dyn Platform) {
        let i = self.library.add(uri, name);
        self.select_tab(i + 1);
        if self.shelf.scanning.insert(uri.to_string()) {
            p.scan_folder(uri);
        }
        if self.doc.is_some() && !self.shelf.open {
            self.show_shelf(p);
        }
    }

    pub(super) fn folder_scanned(&mut self, uri: &str, books: Vec<(String, String, String)>, ok: bool) {
        self.shelf.scanning.remove(uri);
        if ok {
            let books = books.into_iter().map(|(dir, name, uri)| Book { uri, name, dir }).collect();
            self.library.set_books(uri, books);
        } else if let Some(i) = self.library.find(uri) {
            let name = self.library.folders[i].name.clone();
            self.toast(format!("Can't read the folder “{name}”"));
        }
        self.shelf.stale = true;
    }

    pub(super) fn shelf_scroll_by(&mut self, dy: f64) {
        self.shelf.fling = None;
        self.shelf.scroll = (self.shelf.scroll + dy).clamp(0.0, self.shelf_max_scroll());
        self.dirty = true;
    }

    // ----- covers -----------------------------------------------------------------------

    /// A document just opened: cache its cover while PDFium has it open.
    pub(super) fn opened_cover(&mut self, doc: u64, uri: &str) {
        let key = covers::key(uri);
        if let Some(dir) = &self.covers.dir {
            if !self.covers.disk.contains(&key) {
                self.worker.cover(key, CoverSrc::Doc(doc), dir.clone());
            }
        }
    }

    pub(super) fn cover_fd(&mut self, key: u64, fd: i32) {
        match (&self.covers.dir, fd >= 0) {
            (Some(dir), true) => self.worker.cover(key, CoverSrc::Fd(fd), dir.clone()),
            (None, true) => unsafe {
                libc::close(fd);
            },
            _ => self.cover_failed(key, false),
        }
    }

    pub(super) fn cover_ready(&mut self, key: u64, w: u32, h: u32, pixels: Vec<u8>) {
        let c = &mut self.covers;
        c.disk.insert(key);
        c.busy.remove(&key);
        match c.slots.get(&key) {
            Some(Slot::Pending) if self.shelf.open || self.doc.is_none() => c.uploads.push_back((key, w, h, pixels)),
            Some(Slot::Pending | Slot::Failed) => {
                // Not shown now (or failed before but works now): ask again when needed.
                c.slots.remove(&key);
            }
            _ => {}
        }
        self.dirty = true;
    }

    pub(super) fn cover_failed(&mut self, key: u64, missing: bool) {
        let c = &mut self.covers;
        c.busy.remove(&key);
        if missing {
            // Not in the disk cache after all: render it instead.
            c.disk.remove(&key);
            if matches!(c.slots.get(&key), Some(Slot::Pending)) {
                c.slots.remove(&key);
            }
        } else if let Some(s @ Slot::Pending) = c.slots.get_mut(&key) {
            *s = Slot::Failed;
        }
        self.dirty = true;
    }

    /// Asks for the covers on screen (then just below and above) that are not
    /// loaded yet, a few at a time so scrolling stays responsive.
    fn request_covers(&mut self, p: &mut dyn Platform) {
        let Some(dir) = self.covers.dir.clone() else { return };
        if self.covers.busy.len() >= COVER_JOBS {
            return;
        }
        let (s, area) = (self.shelf.scroll, self.shelf_area());
        let ranges = [(s, s + area), (s + area, s + area * 2.0), (s - area * 0.5, s)];
        for (y0, y1) in ranges {
            let first = self.shelf.rows.partition_point(|r| r.y + r.h <= y0);
            for r in &self.shelf.rows[first..] {
                if r.y >= y1 {
                    break;
                }
                let RowKind::Cells(range) = &r.kind else { continue };
                for item in &self.shelf.items[range.clone()] {
                    if self.covers.busy.len() >= COVER_JOBS {
                        return;
                    }
                    let key = item.key;
                    if self.covers.slots.contains_key(&key) {
                        continue;
                    }
                    if self.covers.disk.contains(&key) {
                        self.worker.cover(key, CoverSrc::Disk, dir.clone());
                    } else {
                        p.open_cover(key, &item.uri);
                    }
                    self.covers.slots.insert(key, Slot::Pending);
                    self.covers.busy.insert(key);
                }
            }
        }
    }

    // ----- per frame --------------------------------------------------------------------

    pub(super) fn shelf_update(&mut self, now: f64, p: &mut dyn Platform) {
        if self.shelf.last_scan.is_none() {
            self.scan_all(p);
        }
        if self.shelf.remove_armed.is_some_and(|t| now >= t) {
            self.shelf.remove_armed = None;
        }
        if !self.shelf_visible() {
            return;
        }
        if self.shelf.stale || self.shelf.built != [self.win_w, self.insets[0], self.insets[2]] {
            self.shelf_build();
        }
        if let Some(mut f) = self.shelf.fling.take() {
            let ((_, dy), running) = f.step(now);
            let max = self.shelf_max_scroll();
            let s = self.shelf.scroll - dy;
            self.shelf.scroll = s.clamp(0.0, max);
            if running && s == self.shelf.scroll {
                self.shelf.fling = Some(f);
            }
            self.dirty = true;
        }
        self.request_covers(p);
    }

    // ----- touch ------------------------------------------------------------------------

    pub(super) fn shelf_touch(&mut self, action: i32, index: usize, time: f64, p: &mut dyn Platform) {
        let (x, y) = self.pointers.get(index).map_or((0.0, 0.0), |&(_, x, y)| (x, y));
        let slop = self.dp(8.0);
        match action {
            ACTION_DOWN => {
                self.vel.reset();
                self.vel.add(time, x as f64, y as f64);
                let stopped = self.shelf.fling.take().is_some();
                self.gesture = if let Some(b) = self.buttons.iter().rev().find(|b| inside(b.r, x, y)).copied() {
                    Gesture::Button { hit: b.hit, r: b.r, inside: true }
                } else if y >= self.shelf_top() {
                    // Touching a moving grid only stops it.
                    Gesture::Shelf { y, sx: x, sy: y, moved: stopped }
                } else if y >= self.shelf_tabs_top() {
                    Gesture::ShelfTabs { x, start: x, y, moved: false }
                } else {
                    Gesture::None
                };
            }
            ACTION_MOVE => {
                let Some(&(_, cx, cy)) = self.pointers.first() else { return };
                match self.gesture {
                    Gesture::Button { hit, r, .. } => self.gesture = Gesture::Button { hit, r, inside: inside(r, cx, cy) },
                    Gesture::Shelf { y, sx, sy, moved } => {
                        self.vel.add(time, cx as f64, cy as f64);
                        let moved = moved || (cy - sy).abs() > slop || (cx - sx).abs() > slop;
                        if moved {
                            self.shelf_scroll_by(-(cy - y) as f64);
                        }
                        self.gesture = Gesture::Shelf { y: cy, sx, sy, moved };
                    }
                    Gesture::ShelfTabs { x, start, y, moved } => {
                        let moved = moved || (cx - start).abs() > slop;
                        if moved {
                            self.shelf.tabs_scroll = (self.shelf.tabs_scroll - (cx - x)).clamp(0.0, self.shelf.tabs_max);
                        }
                        self.gesture = Gesture::ShelfTabs { x: cx, start, y, moved };
                    }
                    _ => {}
                }
                self.dirty = true;
            }
            ACTION_UP => match std::mem::replace(&mut self.gesture, Gesture::None) {
                Gesture::Button { hit, r, .. } if inside(r, x, y) => self.press(hit, p),
                Gesture::Shelf { moved: true, .. } => {
                    let (_, vy) = self.vel.velocity(time);
                    self.shelf.fling = Fling::new(self.now, 0.0, vy, self.density as f64);
                }
                Gesture::Shelf { sx, sy, moved: false, .. } => {
                    if let Some(i) = self.shelf_item_at(sx, sy) {
                        self.open_book(i, p);
                    }
                }
                Gesture::ShelfTabs { start, y, moved: false, .. } => {
                    if let Some(i) = self.shelf.chips.iter().position(|&r| inside(r, start, y)) {
                        self.select_tab(i);
                    }
                }
                _ => {}
            },
            ACTION_CANCEL => self.gesture = Gesture::None,
            _ => {}
        }
    }

    // ----- drawing ----------------------------------------------------------------------

    pub(super) fn render_shelf(&mut self, gpu: &mut dyn Gpu) {
        if self.shelf.stale || self.shelf.built != [self.win_w, self.insets[0], self.insets[2]] {
            self.shelf_build();
        }
        let frame = self.frame;
        if self.covers.upload(gpu, frame) {
            self.dirty = true;
        }
        gpu.begin(self.win_w as u32, self.vh as u32, ui::BG);
        let mut under = Painter::default();
        let mut over = Painter::default();
        let mut draws: Vec<(TexId, [f32; 4], [f32; 4])> = Vec::new();
        self.buttons.clear();

        let g = self.shelf_grid();
        let top = self.shelf_top();
        let vh = self.vh;
        let scroll = self.shelf.scroll;
        let pressed = match self.gesture {
            Gesture::Shelf { sx, sy, moved: false, .. } => self.shelf_item_at(sx, sy),
            _ => None,
        };
        let rows = std::mem::take(&mut self.shelf.rows);
        let items = std::mem::take(&mut self.shelf.items);
        let (tpx, spx, hpx, lpx) = (self.dp(13.0), self.dp(11.5), self.dp(15.0), self.dp(30.0));
        let (pad, bar_h, radius) = (self.dp(8.0), self.dp(3.0), self.dp(6.0));
        let first = rows.partition_point(|r| r.y + r.h <= scroll);
        for row in &rows[first..] {
            let y = top + (row.y - scroll) as f32;
            if y > vh {
                break;
            }
            match &row.kind {
                RowKind::Header(title, count) => {
                    let base = y + row.h as f32 / 2.0 + self.dp(4.0);
                    let n = format!("{count}");
                    let nw = self.text.measure(&n, tpx);
                    let right = self.win_w - self.insets[2] - self.dp(16.0);
                    self.text.draw(&mut over, &n, right - nw, base + self.text.cap_height(tpx) / 2.0, tpx, ui::DIM, nw + 1.0);
                    let max = (right - nw - self.dp(12.0) - g.left).max(0.0);
                    self.text.draw(&mut over, title, g.left, base + self.text.cap_height(hpx) / 2.0, hpx, ui::ACCENT, max);
                }
                RowKind::Cells(range) => {
                    for (c, i) in range.clone().enumerate() {
                        let item = &items[i];
                        let x = g.left + c as f32 * (g.cell_w + g.gap);
                        let (cw, ch) = (g.cell_w, g.cover_h);
                        let mut img = [x, y, cw, ch];
                        match self.covers.slots.get_mut(&item.key) {
                            Some(Slot::Ready { tex, w, h, used }) => {
                                *used = frame;
                                let (w, h) = (*w as f32, *h as f32);
                                let k = (cw / w).min(ch / h);
                                let (dw, dh) = ((w * k).round(), (h * k).round());
                                img = [(x + (cw - dw) / 2.0).round(), y + ch - dh, dw, dh];
                                under.rect(img[0] - 1.0, img[1] - 1.0, dw + 2.0, dh + 2.0, [0, 0, 0, 110], 2.0);
                                let clamp = [0.5 / w, 0.5 / h, 1.0 - 0.5 / w, 1.0 - 0.5 / h];
                                draws.push((*tex, img, clamp));
                            }
                            slot => {
                                under.rect(x, y, cw, ch, [38, 38, 44, 255], radius);
                                if !matches!(slot, Some(Slot::Pending)) {
                                    let letter: String = item.title.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default();
                                    let lw = self.text.measure(&letter, lpx);
                                    let base = y + ch / 2.0 + self.text.cap_height(lpx) / 2.0;
                                    self.text.draw(&mut over, &letter, x + (cw - lw) / 2.0, base, lpx, ui::DIM, cw);
                                }
                            }
                        }
                        if let Some((page, pages)) = item.progress {
                            let f = page as f32 / pages.max(1) as f32;
                            let by = img[1] + img[3] - bar_h;
                            over.rect(img[0], by, img[2], bar_h, [0, 0, 0, 150], 0.0);
                            over.rect(img[0], by, img[2] * f, bar_h, ui::ACCENT, 0.0);
                        }
                        if pressed == Some(i) {
                            over.rect(img[0], img[1], img[2], img[3], ui::PRESS, 0.0);
                        }
                        let base = y + ch + pad + self.text.cap_height(tpx) + self.dp(2.0);
                        self.text.draw(&mut over, &item.title, x, base, tpx, ui::FG, cw);
                        if let Some((page, pages)) = item.progress {
                            let s = format!("{page} / {pages}");
                            self.text.draw(&mut over, &s, x, base + self.dp(18.0), spx, ui::DIM, cw);
                        }
                    }
                }
            }
        }
        let empty = items.is_empty();
        self.shelf.rows = rows;
        self.shelf.items = items;
        if empty {
            self.draw_shelf_empty(&mut over);
        }
        self.draw_shelf_header(&mut over);

        if let Some(msg) = self.shelf_opening_label() {
            let px = self.dp(14.0);
            let w = (self.text.measure(&msg, px) + self.dp(32.0)).min(self.win_w - self.dp(32.0));
            let h = self.dp(40.0);
            let (x, y) = ((self.win_w - w) / 2.0, vh - self.insets[3] - self.dp(24.0) - h);
            over.rect(x, y, w, h, [48, 48, 54, 245], h / 2.0);
            let baseline = y + h / 2.0 + self.text.cap_height(px) / 2.0;
            self.text.draw(&mut over, &msg, x + self.dp(16.0), baseline, px, ui::FG, w - self.dp(32.0));
        }
        if self.mode == Mode::Password {
            // The password prompt for a book opened from here.
            over.ox = self.ox;
            self.draw_overlay(&mut over);
        } else {
            if self.menu {
                self.draw_menu(&mut over);
            }
            self.draw_toast(&mut over, 0.0, self.win_w);
        }

        let atlas = self.text.flush(gpu);
        gpu.draw_ui(&under.v, atlas);
        for (tex, r, clamp) in &draws {
            gpu.draw_texture(*tex, *r, [0.0, 0.0, 1.0, 1.0], *clamp);
        }
        gpu.draw_ui(&over.v, atlas);
        self.covers.evict(gpu, frame);
    }

    fn shelf_opening_label(&self) -> Option<String> {
        let (name, since) = match (&self.pending, &self.shelf.opening) {
            (Some(pd), _) => (library::title(&pd.name).to_string(), pd.since),
            (None, Some((name, since))) => (name.clone(), *since),
            _ => return None,
        };
        (self.now - since > 0.3 && self.mode != Mode::Password).then(|| format!("Opening {name}…"))
    }

    fn draw_shelf_header(&mut self, p: &mut Painter) {
        let w = self.win_w;
        let top = self.shelf_top();
        p.rect(0.0, 0.0, w, top, ui::BAR, 0.0);
        p.rect(0.0, top - 1.0, w, 1.0, [255, 255, 255, 18], 0.0);
        let b = self.dp(48.0);
        let cy = self.insets[1] + self.dp(28.0);
        let left = self.insets[0] + self.dp(4.0);
        let mut tx = self.insets[0] + self.dp(20.0);
        if self.doc.is_some() {
            self.icon_button(p, Icon::Back, left, cy, Hit::ShelfBack, 1.0);
            tx = left + b + self.dp(8.0);
        }
        let mut rx = w - self.insets[2] - self.dp(4.0);
        rx -= b;
        self.icon_button(p, Icon::Settings, rx, cy, Hit::Settings, 1.0);
        rx -= b;
        self.icon_button(p, Icon::Open, rx, cy, Hit::ShelfAddFolder, 1.0);
        rx -= b;
        self.icon_button(p, Icon::File, rx, cy, Hit::ShelfOpenFile, 1.0);
        if self.shelf.tab > 0 {
            rx -= b;
            let armed = self.shelf.remove_armed.is_some();
            self.icon_button(p, Icon::Remove, rx, cy, Hit::ShelfRemove, 1.0);
            if armed {
                p.ring(rx + b / 2.0, cy, b / 2.0 - self.dp(4.0), self.dp(2.0), ui::HIT_CURRENT);
            }
        }
        let px = self.dp(20.0);
        self.text.draw(p, "Library", tx, cy + self.text.cap_height(px) / 2.0, px, ui::FG, (rx - tx - self.dp(8.0)).max(0.0));

        // Tabs: recent, then one per folder.
        let tpx = self.dp(14.0);
        let (h, gap, padx) = (self.dp(34.0), self.dp(8.0), self.dp(14.0));
        let y = self.shelf_tabs_top() + (self.dp(52.0) - h) / 2.0 - self.dp(2.0);
        let x0 = self.insets[0] + self.dp(16.0);
        let mut x = x0 - self.shelf.tabs_scroll;
        let mut labels = vec![String::from("Recent")];
        labels.extend(self.library.folders.iter().map(|f| {
            if self.shelf.scanning.contains(&f.uri) {
                format!("{} …", f.name)
            } else {
                f.name.clone()
            }
        }));
        self.shelf.chips.clear();
        let max_label = (w * 0.6).max(self.dp(80.0));
        for (i, label) in labels.iter().enumerate() {
            let lw = self.text.measure(label, tpx).min(max_label);
            let cw = lw + 2.0 * padx;
            let sel = i == self.shelf.tab;
            p.rect(x, y, cw, h, if sel { ui::ACCENT } else { ui::FIELD }, h / 2.0);
            let color = if sel { [16, 24, 40, 255] } else { ui::FG };
            self.text.draw(p, label, x + padx, y + h / 2.0 + self.text.cap_height(tpx) / 2.0, tpx, color, lw + 1.0);
            self.shelf.chips.push([x, y, cw, h]);
            x += cw + gap;
        }
        let total = x + self.shelf.tabs_scroll - gap - x0;
        let visible = w - self.insets[2] - self.dp(16.0) - x0;
        self.shelf.tabs_max = (total - visible).max(0.0);
        self.shelf.tabs_scroll = self.shelf.tabs_scroll.min(self.shelf.tabs_max);
    }

    fn draw_shelf_empty(&mut self, p: &mut Painter) {
        let w = self.win_w;
        let cy = self.shelf_top() + (self.shelf_area() as f32 - self.insets[3]) * 0.4;
        let folder = self.shelf.tab.checked_sub(1).and_then(|i| self.library.folders.get(i));
        let nothing = self.store.recent().is_empty() && self.library.folders.is_empty();
        let (title, sub) = match folder {
            Some(f) if self.shelf.scanning.contains(&f.uri) => ("Looking for PDF files…", ""),
            Some(_) => ("No PDF files in this folder", ""),
            None if nothing => ("Your library is empty", "Open a PDF, or add the folder with your books"),
            None => ("No recently opened books", ""),
        };
        let (px, spx) = (self.dp(18.0), self.dp(14.0));
        let tw = self.text.measure(title, px).min(w - self.dp(32.0));
        self.text.draw(p, title, (w - tw) / 2.0, cy, px, ui::FG, tw + 1.0);
        let sw = self.text.measure(sub, spx).min(w - self.dp(32.0));
        self.text.draw(p, sub, (w - sw) / 2.0, cy + self.dp(28.0), spx, ui::DIM, sw + 1.0);
        if !nothing {
            return;
        }
        // Two big buttons to get started.
        let bh = self.dp(48.0);
        let mut y = cy + self.dp(60.0);
        for (label, hit) in [("Open a file", Hit::ShelfOpenFile), ("Add a folder", Hit::ShelfAddFolder)] {
            let bw = (self.text.measure(label, spx) + self.dp(56.0)).max(self.dp(200.0));
            let r = [(w - bw) / 2.0, y, bw, bh];
            let down = matches!(self.gesture, Gesture::Button { hit: h, inside: true, .. } if h == hit);
            p.rect(r[0], r[1], bw, bh, if down { [255, 255, 255, 50] } else { ui::FIELD }, bh / 2.0);
            let lw = self.text.measure(label, spx);
            self.text.draw(p, label, (w - lw) / 2.0, y + bh / 2.0 + self.text.cap_height(spx) / 2.0, spx, ui::ACCENT, lw + 1.0);
            self.buttons.push(Button { r, hit });
            y += bh + self.dp(12.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_names() {
        assert_eq!(
            name_from_uri("content://com.android.providers.downloads.documents/document/raw%3A%2Fstorage%2FDownload%2FBook%201.pdf"),
            "Book 1.pdf"
        );
        assert_eq!(name_from_uri("content://com.android.externalstorage.documents/document/primary%3AManga"), "Manga");
        assert_eq!(name_from_uri("file:///sdcard/x.pdf"), "x.pdf");
        assert_eq!(name_from_uri("%"), "%");
    }
}
