//! Integration tests against a real PDFium build. They need:
//!   PDFIUM_PATH       path to libpdfium.so for the host (scripts/fetch-pdfium.sh linux-x64)
//!   JUSTPDF_TEST_PDF  a PDF made by scripts/make-test-pdf.py
//! and are skipped when either is missing.

use std::sync::mpsc;
use std::time::Duration;

use justpdf::pdfium::{self, Document};
use justpdf::search;
use justpdf::tiles::TileKey;
use justpdf::worker::{Msg, RenderReq, Worker};

fn test_pdf() -> Option<String> {
    let pdf = std::env::var("JUSTPDF_TEST_PDF").ok()?;
    std::env::var_os("PDFIUM_PATH")?;
    Some(pdf)
}

fn open_fd(path: &str) -> i32 {
    let c = std::ffi::CString::new(path).unwrap();
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
    assert!(fd >= 0);
    fd
}

#[test]
fn document_api() {
    let Some(path) = test_pdf() else { return eprintln!("skipped: PDFIUM_PATH/JUSTPDF_TEST_PDF not set") };
    pdfium::init().unwrap();
    let _g = pdfium::LOCK.lock().unwrap();
    let fd = open_fd(&path);
    let mut doc = Document::open(fd, None).unwrap();
    assert_eq!(doc.sizes().len(), 60);
    assert_eq!(doc.sizes()[0], (595.0, 842.0));
    assert_eq!(doc.sizes()[5], (800.0, 6000.0));

    // Render the top-left of page 1 and check it is not blank.
    let mut px = Vec::new();
    assert!(doc.render(0, 1080, 1528, 0, 0, 512, 512, &mut px));
    assert_eq!(px.len(), 512 * 512 * 4);
    let dark = px.chunks(4).filter(|p| p[0] < 128).count();
    assert!(dark > 500, "expected text pixels, got {dark}");

    // Text and search: page 4 contains "needle" twice.
    let hits = doc
        .with_text(3, |tp| {
            let text = search::fold_page(&tp.chars());
            search::find(&text, &search::fold_query("NEEDLE")).into_iter().map(|(s, n)| tp.rects(s, n)).collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(hits.len(), 2);
    let r = hits[0][0];
    assert!(r[0] < r[2] && r[1] < r[3] && r[3] <= 1.0 && r[1] > 0.85, "{r:?}");

    let outline = doc.outline();
    assert_eq!(outline.len(), 18);
    assert_eq!(outline[0].title, "Chapter 1: The Lorem part");
    assert_eq!((outline[1].page, outline[1].depth), (3, 1));
    assert!((outline[1].y.unwrap() - 0.5).abs() < 0.01);
    drop(doc);
    unsafe { libc::close(fd) };
}

#[test]
fn worker_round_trip() {
    let Some(path) = test_pdf() else { return eprintln!("skipped: PDFIUM_PATH/JUSTPDF_TEST_PDF not set") };
    let (tx, rx) = mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    let worker = Worker::new(Box::new(move |m| {
        let _ = tx.lock().unwrap().send(m);
    }));
    worker.open(7, open_fd(&path));
    let next = || rx.recv_timeout(Duration::from_secs(10)).expect("worker reply");
    match next() {
        Msg::Opened { doc, sizes } => assert_eq!((doc, sizes.len()), (7, 60)),
        m => panic!("unexpected {m:?}"),
    }
    assert!(matches!(next(), Msg::Outline { doc: 7, .. }));

    let key = TileKey { page: 2, level: 0, row: 0, col: 0 };
    worker.set_queue(7, vec![RenderReq { key, page_w: 540, page_h: 764, x: 0, y: 0, w: 540, h: 512, urgent: true }]);
    match next() {
        Msg::Tile { doc, key: k, w, h, pixels } => {
            assert_eq!((doc, k, w, h, pixels.len()), (7, key, 540, 512, 540 * 512 * 4));
        }
        m => panic!("unexpected {m:?}"),
    }

    worker.search(7, 1, search::fold_query("needle"), 0, 60);
    let mut hits = 0;
    loop {
        match next() {
            Msg::SearchHits { gen: 1, page, hits: h, .. } => {
                assert_eq!(page, 3);
                hits += h.len();
            }
            Msg::SearchProgress { gen: 1, scanned, total, .. } if scanned == total => break,
            _ => {}
        }
    }
    assert_eq!(hits, 2);
}
