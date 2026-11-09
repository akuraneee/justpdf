//! The PDF worker thread. Owns every PDFium object and serves a priority
//! queue: control jobs, then visible tiles, then search steps, then prefetch.
//!
//! The app replaces the whole render queue whenever the view changes, so
//! requests for tiles that scrolled away simply disappear.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

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
    /// A rendered tile; `req` is the request it answers (its geometry tells
    /// the app whether the tile still fits the current layout).
    Tile {
        doc: u64,
        req: RenderReq,
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

    /// Opens a new document from `fd`. The current document stays usable
    /// until the app sends its first request for the new one.
    pub fn open(&self, doc: u64, fd: OwnedFd) {
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

/// An open document and the descriptor it reads from (dropped in that order).
struct Loaded {
    id: u64,
    doc: Document,
    _fd: OwnedFd,
}

struct State {
    sink: Sink,
    /// The document the app shows: the target of its requests.
    cur: Option<Loaded>,
    /// The most recently opened document, not requested by the app yet. It
    /// replaces `cur` on its first request; until then `cur` stays usable, so
    /// the app can keep its document if it abandons this one (e.g. another
    /// file was picked meanwhile and failed to open).
    fresh: Option<Loaded>,
    /// A document waiting for a password.
    pending: Option<(u64, OwnedFd)>,
    /// Folded text per page of `cur`.
    texts: HashMap<u32, PageText>,
}

impl State {
    /// The document `id`, if open; promotes `fresh` to current on first use.
    fn doc(&mut self, id: u64) -> Option<&mut Document> {
        if self.fresh.as_ref().is_some_and(|f| f.id == id) {
            self.cur = self.fresh.take();
            self.texts.clear();
        }
        self.cur.as_mut().filter(|c| c.id == id).map(|c| &mut c.doc)
    }
}

fn run(shared: Arc<Shared>, sink: Sink) {
    if let Err(e) = pdfium::init() {
        crate::log_e!("pdfium init failed: {e}");
    }
    let mut st = State { sink, cur: None, fresh: None, pending: None, texts: HashMap::new() };
    while let Some(job) = next_job(&shared) {
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
                st.fresh = None;
                st.pending = None;
                st.texts.clear();
            }
            Job::Render(doc, req, mut buf) => {
                let ok = match st.doc(doc) {
                    Some(d) => d.render(req.key.page as usize, req.page_w, req.page_h, req.x, req.y, req.w, req.h, &mut buf),
                    None => false,
                };
                if ok {
                    (st.sink)(Msg::Tile { doc, req, pixels: buf });
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
        }
    }
    let _guard = pdfium::LOCK.lock().unwrap_or_else(|e| e.into_inner());
    drop(st);
}

fn try_open(st: &mut State, doc: u64, fd: OwnedFd, password: Option<&str>) {
    match Document::open(fd.as_raw_fd(), password) {
        Ok(d) => {
            let sizes = d.sizes().to_vec();
            (st.sink)(Msg::Opened { doc, sizes });
            let items = d.outline();
            st.fresh = Some(Loaded { id: doc, doc: d, _fd: fd });
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

fn search_page(st: &mut State, doc: u64, gen: u64, query: &[char], page: u32) {
    if st.doc(doc).is_none() {
        return;
    }
    let State { cur: Some(Loaded { doc: d, .. }), texts, sink, .. } = st else { return };
    let cached = texts.get(&page);
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
                    texts.insert(page, text);
                    Some(rects)
                }
                None => None,
            }
        }
    };
    if let Some(hits) = hits {
        let hits: Vec<_> = hits.into_iter().filter(|r| !r.is_empty()).collect();
        if !hits.is_empty() {
            sink(Msg::SearchHits { doc, gen, page, hits });
        }
    }
}
