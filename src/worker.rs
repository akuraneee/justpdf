//! The PDF worker thread. Owns every PDFium object and serves a priority
//! queue: control jobs, then visible tiles, then library covers, then search
//! steps, then prefetch.
//!
//! The app replaces the whole render queue whenever the view changes, so
//! requests for tiles that scrolled away simply disappear.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::covers;
use crate::pdfium::{self, Document, OpenError, OutlineItem};
use crate::search::{self, PageText};
use crate::tiles::TileKey;

#[derive(Clone, Debug, PartialEq)]
pub struct RenderReq {
    pub key: TileKey,
    pub page_w: u32,
    pub page_h: u32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub urgent: bool,
}

#[derive(Debug)]
pub enum Msg {
    Opened {
        doc: u64,
        sizes: Vec<(f32, f32)>,
    },
    NeedPassword {
        doc: u64,
        wrong: bool,
    },
    OpenFailed {
        doc: u64,
        error: String,
    },
    Outline {
        doc: u64,
        items: Vec<OutlineItem>,
    },
    Tile {
        doc: u64,
        key: TileKey,
        w: u32,
        h: u32,
        pixels: Vec<u8>,
    },
    /// Matches on one page; each match is a list of page-fraction rects.
    SearchHits {
        doc: u64,
        gen: u64,
        page: u32,
        hits: Vec<Vec<[f32; 4]>>,
    },
    SearchProgress {
        doc: u64,
        gen: u64,
        scanned: u32,
        total: u32,
    },
    /// A library cover (RGBA), loaded from the disk cache or freshly rendered
    /// (and then cached).
    Cover {
        key: u64,
        w: u32,
        h: u32,
        pixels: Vec<u8>,
    },
    /// The cover could not be made; `missing`: it was asked from the disk cache
    /// but is not there (any more).
    CoverFailed {
        key: u64,
        missing: bool,
    },
}

/// Where a cover comes from. Every job answers with `Msg::Cover` or `Msg::CoverFailed`.
pub enum CoverSrc {
    /// The disk cache.
    Disk,
    /// The first page of the document behind `fd` (ownership is taken).
    Fd(i32),
    /// The first page of the open document `doc`.
    Doc(u64),
}

struct CoverJob {
    key: u64,
    src: CoverSrc,
    dir: PathBuf,
}

impl Drop for CoverJob {
    fn drop(&mut self) {
        if let CoverSrc::Fd(fd) = std::mem::replace(&mut self.src, CoverSrc::Disk) {
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
        }
    }
}

enum Ctrl {
    Open { doc: u64, fd: OwnedFd },
    Password { doc: u64, password: String },
    Close,
}

struct SearchJob {
    doc: u64,
    gen: u64,
    query: Arc<Vec<char>>,
    next: u32,
    remaining: u32,
    total: u32,
}

#[derive(Default)]
struct Queue {
    quit: bool,
    ctrl: VecDeque<Ctrl>,
    doc: u64,
    render: VecDeque<RenderReq>,
    search: Option<SearchJob>,
    covers: VecDeque<CoverJob>,
    pool: Vec<Vec<u8>>,
}

struct Shared {
    q: Mutex<Queue>,
    cv: Condvar,
}

pub type Sink = Box<dyn Fn(Msg) + Send>;

pub struct Worker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn new(sink: Sink) -> Worker {
        let shared = Arc::new(Shared { q: Mutex::new(Queue::default()), cv: Condvar::new() });
        let s = shared.clone();
        let thread = std::thread::Builder::new().name("pdf-worker".into()).spawn(move || run(s, sink)).expect("spawn worker");
        Worker { shared, thread: Some(thread) }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Queue) -> R) -> R {
        let mut q = self.shared.q.lock().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut q);
        self.shared.cv.notify_one();
        r
    }

    /// Opens a new document from `fd` (ownership is taken). The current
    /// document stays usable until the new one opens successfully.
    pub fn open(&self, doc: u64, fd: i32) {
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        self.with(|q| q.ctrl.push_back(Ctrl::Open { doc, fd }));
    }

    pub fn password(&self, doc: u64, password: String) {
        self.with(|q| q.ctrl.push_back(Ctrl::Password { doc, password }));
    }

    pub fn close(&self) {
        self.with(|q| {
            q.render.clear();
            q.search = None;
            q.ctrl.push_back(Ctrl::Close);
        });
    }

    /// Replaces the render queue (highest priority first) for document `doc`.
    pub fn set_queue(&self, doc: u64, reqs: Vec<RenderReq>) {
        self.with(|q| {
            q.doc = doc;
            q.render = reqs.into();
        });
    }

    pub fn search(&self, doc: u64, gen: u64, query: Vec<char>, start_page: u32, pages: u32) {
        if pages == 0 || query.is_empty() {
            return self.cancel_search();
        }
        self.with(|q| {
            q.search = Some(SearchJob {
                doc,
                gen,
                query: Arc::new(query),
                next: start_page.min(pages.saturating_sub(1)),
                remaining: pages,
                total: pages,
            });
        });
    }

    /// Asks for the cover `key` (cached in `dir`).
    pub fn cover(&self, key: u64, src: CoverSrc, dir: PathBuf) {
        self.with(|q| q.covers.push_back(CoverJob { key, src, dir }));
    }

    pub fn cancel_search(&self) {
        self.with(|q| q.search = None);
    }

    /// Returns a pixel buffer for reuse.
    pub fn recycle(&self, buf: Vec<u8>) {
        let mut q = self.shared.q.lock().unwrap_or_else(|e| e.into_inner());
        if q.pool.len() < 8 {
            q.pool.push(buf);
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.with(|q| q.quit = true);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum Job {
    Ctrl(Ctrl),
    Render(u64, RenderReq, Vec<u8>),
    Search { doc: u64, gen: u64, query: Arc<Vec<char>>, page: u32, last: bool, scanned: u32, total: u32 },
    Cover(CoverJob),
}

fn next_job(shared: &Shared) -> Option<Job> {
    let mut q = shared.q.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if q.quit {
            return None;
        }
        if let Some(c) = q.ctrl.pop_front() {
            return Some(Job::Ctrl(c));
        }
        let urgent = q.render.front().is_some_and(|r| r.urgent);
        if !urgent {
            if let Some(c) = q.covers.pop_front() {
                return Some(Job::Cover(c));
            }
            if let Some(s) = q.search.as_mut() {
                let page = s.next;
                s.next = (s.next + 1) % s.total.max(1);
                s.remaining -= 1;
                let (doc, gen, query, last, scanned, total) =
                    (s.doc, s.gen, s.query.clone(), s.remaining == 0, s.total - s.remaining, s.total);
                if last {
                    q.search = None;
                }
                return Some(Job::Search { doc, gen, query, page, last, scanned, total });
            }
        }
        if let Some(r) = q.render.pop_front() {
            let buf = q.pool.pop().unwrap_or_default();
            return Some(Job::Render(q.doc, r, buf));
        }
        q = shared.cv.wait(q).unwrap_or_else(|e| e.into_inner());
    }
}

struct State {
    sink: Sink,
    /// The document currently shown.
    cur: Option<(u64, Document, OwnedFd)>,
    /// A document waiting for a password.
    pending: Option<(u64, OwnedFd)>,
    texts: HashMap<u32, PageText>,
    /// Covers written since the disk cache was last pruned.
    written: u32,
}

fn run(shared: Arc<Shared>, sink: Sink) {
    if let Err(e) = pdfium::init() {
        crate::log_e!("pdfium init failed: {e}");
    }
    let mut st = State { sink, cur: None, pending: None, texts: HashMap::new(), written: 0 };
    while let Some(job) = next_job(&shared) {
        if let Job::Cover(CoverJob { key, src: CoverSrc::Disk, dir }) = &job {
            // Plain file I/O: no PDFium lock needed.
            match covers::load(dir, *key) {
                Some((w, h, pixels)) => (st.sink)(Msg::Cover { key: *key, w, h, pixels }),
                None => (st.sink)(Msg::CoverFailed { key: *key, missing: true }),
            }
            continue;
        }
        let _guard = pdfium::LOCK.lock().unwrap_or_else(|e| e.into_inner());
        match job {
            Job::Ctrl(Ctrl::Open { doc, fd }) => {
                st.pending = None;
                try_open(&mut st, doc, fd, None);
            }
            Job::Ctrl(Ctrl::Password { doc, password }) => {
                if let Some((pdoc, fd)) = st.pending.take() {
                    if pdoc == doc {
                        try_open(&mut st, doc, fd, Some(&password));
                    }
                }
            }
            Job::Ctrl(Ctrl::Close) => {
                st.cur = None;
                st.pending = None;
                st.texts.clear();
            }
            Job::Render(doc, req, mut buf) => {
                let ok = match st.cur.as_mut() {
                    Some((id, d, _)) if *id == doc => {
                        d.render(req.key.page as usize, req.page_w, req.page_h, req.x, req.y, req.w, req.h, &mut buf)
                    }
                    _ => false,
                };
                if ok {
                    (st.sink)(Msg::Tile { doc, key: req.key, w: req.w, h: req.h, pixels: buf });
                } else {
                    let mut q = shared.q.lock().unwrap_or_else(|e| e.into_inner());
                    q.pool.push(buf);
                }
            }
            Job::Search { doc, gen, query, page, last, scanned, total } => {
                search_page(&mut st, doc, gen, &query, page);
                if last || scanned % 16 == 0 {
                    (st.sink)(Msg::SearchProgress { doc, gen, scanned, total });
                }
            }
            Job::Cover(job) => make_cover(&mut st, job),
        }
    }
    let _guard = pdfium::LOCK.lock().unwrap_or_else(|e| e.into_inner());
    st.cur = None;
}

fn try_open(st: &mut State, doc: u64, fd: OwnedFd, password: Option<&str>) {
    match Document::open(fd.as_raw_fd(), password) {
        Ok(d) => {
            let sizes = d.sizes().to_vec();
            (st.sink)(Msg::Opened { doc, sizes });
            let items = d.outline();
            st.texts.clear();
            st.cur = Some((doc, d, fd));
            (st.sink)(Msg::Outline { doc, items });
        }
        Err(OpenError::Password) => {
            st.pending = Some((doc, fd));
            (st.sink)(Msg::NeedPassword { doc, wrong: password.is_some() });
        }
        Err(OpenError::Other(error)) => {
            crate::log_w!("open failed: {error}");
            (st.sink)(Msg::OpenFailed { doc, error });
        }
    }
}

/// Renders the first page of a document as a cover, caches it and sends it.
fn make_cover(st: &mut State, mut job: CoverJob) {
    let key = job.key;
    let pixels = match std::mem::replace(&mut job.src, CoverSrc::Disk) {
        CoverSrc::Disk => None,
        CoverSrc::Fd(fd) => {
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            Document::open(fd.as_raw_fd(), None).ok().and_then(|mut d| render_cover(&mut d))
        }
        CoverSrc::Doc(doc) => match st.cur.as_mut() {
            Some((id, d, _)) if *id == doc => render_cover(d),
            _ => None,
        },
    };
    match pixels {
        Some((w, h, px)) => {
            covers::save(&job.dir, key, w, h, &px);
            st.written += 1;
            if st.written % 32 == 1 {
                covers::prune(&job.dir);
            }
            (st.sink)(Msg::Cover { key, w, h, pixels: px });
        }
        None => (st.sink)(Msg::CoverFailed { key, missing: false }),
    }
}

fn render_cover(d: &mut Document) -> Option<(u32, u32, Vec<u8>)> {
    let &(pw, ph) = d.sizes().first()?;
    let (page_w, page_h, h) = covers::dims(pw, ph);
    let mut px = Vec::new();
    d.render(0, page_w, page_h, 0, 0, page_w, h, &mut px).then_some((page_w, h, px))
}

fn search_page(st: &mut State, doc: u64, gen: u64, query: &[char], page: u32) {
    let Some((id, d, _)) = st.cur.as_mut() else { return };
    if *id != doc {
        return;
    }
    let cached = st.texts.get(&page);
    let hits = match cached {
        Some(text) => {
            let ranges = search::find(text, query);
            if ranges.is_empty() {
                return;
            }
            d.with_text(page as usize, |tp| ranges.iter().map(|&(s, n)| tp.rects(s, n)).collect::<Vec<_>>())
        }
        None => {
            let res = d.with_text(page as usize, |tp| {
                let text = search::fold_page(&tp.chars());
                let ranges = search::find(&text, query);
                let rects: Vec<_> = ranges.iter().map(|&(s, n)| tp.rects(s, n)).collect();
                (text, rects)
            });
            match res {
                Some((text, rects)) => {
                    st.texts.insert(page, text);
                    Some(rects)
                }
                None => None,
            }
        }
    };
    if let Some(hits) = hits {
        let hits: Vec<_> = hits.into_iter().filter(|r| !r.is_empty()).collect();
        if !hits.is_empty() {
            (st.sink)(Msg::SearchHits { doc, gen, page, hits });
        }
    }
}
