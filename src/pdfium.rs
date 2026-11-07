//! Thin bindings to PDFium (loaded with `dlopen`, so the crate links without it)
//! and a small wrapper around the handful of calls the reader needs.
//!
//! PDFium is not thread safe: every call must be made while holding [`LOCK`].
//! In practice only the worker thread touches it.

use std::collections::HashSet;
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CString};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};

type H = *mut c_void;

/// Serialises all PDFium usage in the process.
pub static LOCK: Mutex<()> = Mutex::new(());

const FPDF_ANNOT: c_int = 0x01;
const FPDF_REVERSE_BYTE_ORDER: c_int = 0x10;
const FPDF_ERR_PASSWORD: c_ulong = 4;
const FPDFBITMAP_BGRX: c_int = 3;
const PDFACTION_GOTO: c_ulong = 1;

#[repr(C)]
struct FileAccess {
    len: c_ulong,
    get_block: unsafe extern "C" fn(*mut c_void, c_ulong, *mut u8, c_ulong) -> c_int,
    param: *mut c_void,
}

#[repr(C)]
struct LibraryConfig {
    version: c_int,
    user_font_paths: *const *const c_char,
    isolate: *mut c_void,
    v8_embedder_slot: c_uint,
}

#[repr(C)]
struct FsMatrix {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

#[repr(C)]
struct FsRectF {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

#[repr(C)]
#[derive(Default)]
struct FsSizeF {
    width: f32,
    height: f32,
}

macro_rules! api {
    ($( fn $name:ident ( $($arg:ty),* ) $(-> $ret:ty)? ; )*) => {
        #[allow(non_snake_case)]
        struct Api { $( $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )* }
        impl Api {
            unsafe fn load(lib: *mut c_void) -> Result<Api, String> {
                Ok(Api { $( $name: {
                    let sym = libc::dlsym(lib, concat!(stringify!($name), "\0").as_ptr().cast());
                    if sym.is_null() {
                        return Err(concat!("pdfium: missing symbol ", stringify!($name)).to_string());
                    }
                    std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(sym)
                }, )* })
            }
        }
    };
}

api! {
    fn FPDF_InitLibraryWithConfig(*const LibraryConfig);
    fn FPDF_LoadCustomDocument(*mut FileAccess, *const c_char) -> H;
    fn FPDF_CloseDocument(H);
    fn FPDF_GetLastError() -> c_ulong;
    fn FPDF_GetPageCount(H) -> c_int;
    fn FPDF_GetPageSizeByIndexF(H, c_int, *mut FsSizeF) -> c_int;
    fn FPDF_LoadPage(H, c_int) -> H;
    fn FPDF_ClosePage(H);
    fn FPDF_RenderPageBitmapWithMatrix(H, H, *const FsMatrix, *const FsRectF, c_int);
    fn FPDF_PageToDevice(H, c_int, c_int, c_int, c_int, c_int, f64, f64, *mut c_int, *mut c_int) -> c_int;
    fn FPDFBitmap_CreateEx(c_int, c_int, c_int, *mut c_void, c_int) -> H;
    fn FPDFBitmap_FillRect(H, c_int, c_int, c_int, c_int, c_ulong) -> c_int;
    fn FPDFBitmap_Destroy(H);
    fn FPDFText_LoadPage(H) -> H;
    fn FPDFText_ClosePage(H);
    fn FPDFText_CountChars(H) -> c_int;
    fn FPDFText_GetUnicode(H, c_int) -> c_uint;
    fn FPDFText_CountRects(H, c_int, c_int) -> c_int;
    fn FPDFText_GetRect(H, c_int, *mut f64, *mut f64, *mut f64, *mut f64) -> c_int;
    fn FPDFBookmark_GetFirstChild(H, H) -> H;
    fn FPDFBookmark_GetNextSibling(H, H) -> H;
    fn FPDFBookmark_GetTitle(H, *mut c_void, c_ulong) -> c_ulong;
    fn FPDFBookmark_GetDest(H, H) -> H;
    fn FPDFBookmark_GetAction(H) -> H;
    fn FPDFAction_GetType(H) -> c_ulong;
    fn FPDFAction_GetDest(H, H) -> H;
    fn FPDFDest_GetDestPageIndex(H, H) -> c_int;
    fn FPDFDest_GetLocationInPage(H, *mut c_int, *mut c_int, *mut c_int, *mut f32, *mut f32, *mut f32) -> c_int;
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> Result<&'static Api, String> {
    API.get_or_init(|| unsafe { load_library() }).as_ref().map_err(Clone::clone)
}

/// Loads and initialises PDFium. Cheap to call repeatedly; the first call does the work.
pub fn init() -> Result<(), String> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    api().map(|_| ())
}

unsafe fn load_library() -> Result<Api, String> {
    let path = std::env::var("PDFIUM_PATH").unwrap_or_else(|_| "libpdfium.so".to_string());
    let cpath = CString::new(path.clone()).map_err(|e| e.to_string())?;
    let lib = libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
    if lib.is_null() {
        let err = libc::dlerror();
        let msg = if err.is_null() { String::from("unknown error") } else { std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned() };
        return Err(format!("dlopen({path}) failed: {msg}"));
    }
    let api = Api::load(lib)?;
    // PDFium keeps this pointer and scans the folders lazily, so it must live forever.
    let font_paths: *const *const c_char = if cfg!(target_os = "android") {
        let paths: Vec<*const c_char> = vec![c"/system/fonts/".as_ptr(), null()];
        Box::leak(paths.into_boxed_slice()).as_ptr()
    } else {
        null()
    };
    let config = LibraryConfig { version: 2, user_font_paths: font_paths, isolate: null_mut(), v8_embedder_slot: 0 };
    (api.FPDF_InitLibraryWithConfig)(&config);
    Ok(api)
}

#[derive(Debug)]
pub enum OpenError {
    Password,
    Other(String),
}

/// Where PDFium reads bytes from. Regular files are read lazily with `pread`.
enum Source {
    Fd(c_int),
    Mem(Vec<u8>),
}

unsafe extern "C" fn get_block(param: *mut c_void, pos: c_ulong, buf: *mut u8, size: c_ulong) -> c_int {
    let source = &*(param as *const Source);
    let size = size as usize;
    match source {
        Source::Fd(fd) => {
            let mut done = 0usize;
            while done < size {
                #[allow(clippy::useless_conversion)] // c_ulong is u32 on 32-bit targets
                let offset = u64::from(pos) + done as u64;
                let r = libc::pread64(*fd, buf.add(done).cast(), size - done, offset as libc::off64_t);
                if r < 0 {
                    if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return 0;
                }
                if r == 0 {
                    return 0;
                }
                done += r as usize;
            }
            1
        }
        Source::Mem(data) => {
            let start = pos as usize;
            match start.checked_add(size) {
                Some(end) if end <= data.len() => {
                    std::ptr::copy_nonoverlapping(data.as_ptr().add(start), buf, size);
                    1
                }
                _ => 0,
            }
        }
    }
}

fn make_source(fd: c_int) -> Result<(Source, usize), OpenError> {
    let size = unsafe { libc::lseek64(fd, 0, libc::SEEK_END) };
    if size > 0 {
        return Ok((Source::Fd(fd), size as usize));
    }
    // Not seekable (a pipe from some content providers): slurp it into memory.
    let mut data = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        let r = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if r < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(OpenError::Other("read failed".into()));
        }
        if r == 0 {
            break;
        }
        data.extend_from_slice(&chunk[..r as usize]);
    }
    if data.is_empty() {
        return Err(OpenError::Other("empty file".into()));
    }
    let len = data.len();
    Ok((Source::Mem(data), len))
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutlineItem {
    pub title: String,
    pub page: u32,
    /// Target position inside the page, as a fraction from the top.
    pub y: Option<f32>,
    pub depth: u8,
}

const PAGE_CACHE: usize = 4;

pub struct Document {
    api: &'static Api,
    doc: H,
    sizes: Vec<(f32, f32)>,
    /// Recently used pages, most recent last. Keeping pages open lets PDFium
    /// reuse decoded images between tiles of the same page.
    pages: Vec<(usize, H)>,
    _access: Box<FileAccess>,
    _source: Box<Source>,
}

// SAFETY: the handles are only used by whichever thread owns the Document,
// and all PDFium calls are serialised through LOCK.
unsafe impl Send for Document {}

impl Document {
    /// Opens a document reading from `fd`. The descriptor is borrowed and must
    /// stay open for the lifetime of the document.
    pub fn open(fd: c_int, password: Option<&str>) -> Result<Document, OpenError> {
        let api = api().map_err(OpenError::Other)?;
        let (source, len) = make_source(fd)?;
        let source = Box::new(source);
        let mut access = Box::new(FileAccess { len: len as c_ulong, get_block, param: &*source as *const Source as *mut c_void });
        let password = password.map(|p| CString::new(p).unwrap_or_default());
        let doc = unsafe { (api.FPDF_LoadCustomDocument)(&mut *access, password.as_ref().map_or(null(), |p| p.as_ptr())) };
        if doc.is_null() {
            let err = unsafe { (api.FPDF_GetLastError)() };
            return Err(if err == FPDF_ERR_PASSWORD { OpenError::Password } else { OpenError::Other(format!("pdfium error {err}")) });
        }
        let count = unsafe { (api.FPDF_GetPageCount)(doc) }.max(0);
        if count == 0 {
            unsafe { (api.FPDF_CloseDocument)(doc) };
            return Err(OpenError::Other("document has no pages".into()));
        }
        let sizes = (0..count)
            .map(|i| {
                let mut s = FsSizeF::default();
                let ok = unsafe { (api.FPDF_GetPageSizeByIndexF)(doc, i, &mut s) } != 0;
                if ok && s.width > 0.5 && s.height > 0.5 && s.width.is_finite() && s.height.is_finite() {
                    (s.width, s.height)
                } else {
                    (612.0, 792.0)
                }
            })
            .collect();
        Ok(Document { api, doc, sizes, pages: Vec::new(), _access: access, _source: source })
    }

    pub fn sizes(&self) -> &[(f32, f32)] {
        &self.sizes
    }

    fn page(&mut self, index: usize) -> Option<H> {
        if let Some(pos) = self.pages.iter().position(|(i, _)| *i == index) {
            let entry = self.pages.remove(pos);
            self.pages.push(entry);
            return Some(entry.1);
        }
        let page = unsafe { (self.api.FPDF_LoadPage)(self.doc, index as c_int) };
        if page.is_null() {
            return None;
        }
        if self.pages.len() >= PAGE_CACHE {
            let (_, old) = self.pages.remove(0);
            unsafe { (self.api.FPDF_ClosePage)(old) };
        }
        self.pages.push((index, page));
        Some(page)
    }

    /// Renders the region (`x`, `y`, `w`, `h`) of page `index` scaled to
    /// `page_w` x `page_h` pixels into `out` as RGBX.
    #[allow(clippy::too_many_arguments)]
    pub fn render(&mut self, index: usize, page_w: u32, page_h: u32, x: u32, y: u32, w: u32, h: u32, out: &mut Vec<u8>) -> bool {
        let Some(&(pw, ph)) = self.sizes.get(index) else { return false };
        let Some(page) = self.page(index) else { return false };
        out.resize(w as usize * h as usize * 4, 0);
        let api = self.api;
        unsafe {
            let bitmap = (api.FPDFBitmap_CreateEx)(w as c_int, h as c_int, FPDFBITMAP_BGRX, out.as_mut_ptr().cast(), (w * 4) as c_int);
            if bitmap.is_null() {
                return false;
            }
            (api.FPDFBitmap_FillRect)(bitmap, 0, 0, w as c_int, h as c_int, 0xFFFF_FFFF);
            let matrix = FsMatrix { a: page_w as f32 / pw, b: 0.0, c: 0.0, d: page_h as f32 / ph, e: -(x as f32), f: -(y as f32) };
            let clip = FsRectF { left: 0.0, top: 0.0, right: w as f32, bottom: h as f32 };
            (api.FPDF_RenderPageBitmapWithMatrix)(bitmap, page, &matrix, &clip, FPDF_ANNOT | FPDF_REVERSE_BYTE_ORDER);
            (api.FPDFBitmap_Destroy)(bitmap);
        }
        true
    }

    /// Runs `f` with the text layer of page `index`. Does not disturb the page cache.
    pub fn with_text<R>(&mut self, index: usize, f: impl FnOnce(&TextPage) -> R) -> Option<R> {
        let &(pw, ph) = self.sizes.get(index)?;
        let api = self.api;
        let cached = self.pages.iter().find(|(i, _)| *i == index).map(|(_, p)| *p);
        let (page, owned) = match cached {
            Some(p) => (p, false),
            None => {
                let p = unsafe { (api.FPDF_LoadPage)(self.doc, index as c_int) };
                if p.is_null() {
                    return None;
                }
                (p, true)
            }
        };
        let tp = unsafe { (api.FPDFText_LoadPage)(page) };
        let result = if tp.is_null() {
            None
        } else {
            let text = TextPage { api, page, tp, size: (pw, ph) };
            let r = f(&text);
            unsafe { (api.FPDFText_ClosePage)(tp) };
            Some(r)
        };
        if owned {
            unsafe { (api.FPDF_ClosePage)(page) };
        }
        result
    }

    pub fn outline(&self) -> Vec<OutlineItem> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        self.walk_outline(null_mut(), 0, &mut out, &mut seen);
        out
    }

    fn walk_outline(&self, parent: H, depth: u8, out: &mut Vec<OutlineItem>, seen: &mut HashSet<usize>) {
        let api = self.api;
        let mut bm = unsafe { (api.FPDFBookmark_GetFirstChild)(self.doc, parent) };
        while !bm.is_null() && out.len() < 50_000 {
            if !seen.insert(bm as usize) {
                break;
            }
            let title = self.bookmark_title(bm);
            let (page, y) = self.bookmark_target(bm);
            let page = page.or_else(|| out.last().map(|i: &OutlineItem| i.page)).unwrap_or(0);
            out.push(OutlineItem { title, page, y, depth });
            if depth < 32 {
                self.walk_outline(bm, depth + 1, out, seen);
            }
            bm = unsafe { (api.FPDFBookmark_GetNextSibling)(self.doc, bm) };
        }
    }

    fn bookmark_title(&self, bm: H) -> String {
        let api = self.api;
        let bytes = unsafe { (api.FPDFBookmark_GetTitle)(bm, null_mut(), 0) } as usize;
        if bytes <= 2 {
            return String::new();
        }
        let mut buf = vec![0u16; bytes / 2];
        unsafe { (api.FPDFBookmark_GetTitle)(bm, buf.as_mut_ptr().cast(), (buf.len() * 2) as c_ulong) };
        while buf.last() == Some(&0) {
            buf.pop();
        }
        String::from_utf16_lossy(&buf).split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn bookmark_target(&self, bm: H) -> (Option<u32>, Option<f32>) {
        let api = self.api;
        unsafe {
            let mut dest = (api.FPDFBookmark_GetDest)(self.doc, bm);
            if dest.is_null() {
                let action = (api.FPDFBookmark_GetAction)(bm);
                if !action.is_null() && (api.FPDFAction_GetType)(action) == PDFACTION_GOTO {
                    dest = (api.FPDFAction_GetDest)(self.doc, action);
                }
            }
            if dest.is_null() {
                return (None, None);
            }
            let page = (api.FPDFDest_GetDestPageIndex)(self.doc, dest);
            if page < 0 || page as usize >= self.sizes.len() {
                return (None, None);
            }
            let (mut has_x, mut has_y, mut has_zoom) = (0, 0, 0);
            let (mut x, mut y, mut zoom) = (0f32, 0f32, 0f32);
            let ok = (api.FPDFDest_GetLocationInPage)(dest, &mut has_x, &mut has_y, &mut has_zoom, &mut x, &mut y, &mut zoom);
            let ph = self.sizes[page as usize].1;
            let frac = (ok != 0 && has_y != 0 && y.is_finite()).then(|| (1.0 - y / ph).clamp(0.0, 1.0));
            (Some(page as u32), frac)
        }
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        unsafe {
            for (_, page) in self.pages.drain(..) {
                (self.api.FPDF_ClosePage)(page);
            }
            (self.api.FPDF_CloseDocument)(self.doc);
        }
    }
}

pub struct TextPage {
    api: &'static Api,
    page: H,
    tp: H,
    size: (f32, f32),
}

impl TextPage {
    /// Unicode code points of every character on the page (PDFium char indices).
    pub fn chars(&self) -> Vec<u32> {
        let n = unsafe { (self.api.FPDFText_CountChars)(self.tp) }.max(0);
        (0..n).map(|i| unsafe { (self.api.FPDFText_GetUnicode)(self.tp, i) }).collect()
    }

    /// Boxes covering chars `start..start+count`, as fractions of the page
    /// (`[x0, y0, x1, y1]`, origin top-left).
    pub fn rects(&self, start: usize, count: usize) -> Vec<[f32; 4]> {
        let api = self.api;
        let n = unsafe { (api.FPDFText_CountRects)(self.tp, start as c_int, count as c_int) }.max(0);
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            let (mut l, mut t, mut r, mut b) = (0f64, 0f64, 0f64, 0f64);
            if unsafe { (api.FPDFText_GetRect)(self.tp, i, &mut l, &mut t, &mut r, &mut b) } == 0 {
                continue;
            }
            let (x0, y0) = self.to_unit(l, t);
            let (x1, y1) = self.to_unit(r, b);
            out.push([x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]);
        }
        out
    }

    fn to_unit(&self, x: f64, y: f64) -> (f32, f32) {
        let sx = (self.size.0 * 16.0).round().max(1.0) as c_int;
        let sy = (self.size.1 * 16.0).round().max(1.0) as c_int;
        let (mut dx, mut dy) = (0, 0);
        unsafe { (self.api.FPDF_PageToDevice)(self.page, 0, 0, sx, sy, 0, x, y, &mut dx, &mut dy) };
        (dx as f32 / sx as f32, dy as f32 / sy as f32)
    }
}
