# JustPDF

Minimal, fast Android PDF reader written in Rust: continuous vertical (webtoon) scrolling, optional two-page spreads (settings menu), tabs (open documents, a strip under the top bar) and split view (drag a tab into the other half of the window: two documents — or the same one twice — side by side, one above the other in portrait), chapters (PDF outline), full-text search. Launch → library (cover grid: recently opened documents + folders the user added, grouped by subfolder) → viewer; the system file picker is one button away. A setting ("Library at start", gear button in the reader and library top bars) turns the library off and restores the old flow: launch → system file picker → viewer, cancelling the picker with nothing open exits, the reader's Open button opens the picker. Also opens PDFs via "Open with" / "Share".

## Layout of the repo
```
src/                Rust crate (cdylib libjustpdf.so + rlib for tests)
  android.rs        JNI exports + render thread (EGL, frame loop, Java callbacks)
  app.rs            platform-independent state machine: panes (split view), input, view, tile requests, overlay UI
  app/tabs.rs       document tabs (strip under the top bar), closing tabs, dragging a tab into the other half → split view
  app/shelf.rs      the library screen ("shelf"): tabs (Recent + one per folder), cover grid, cover requests/textures
  library.rs        library folders + their scanned books (files/folders.tsv), natural sort, grouping by subfolder
  covers.rs         book covers: first page at 300 px wide, disk cache files/covers/<fnv64(uri)>.cov (deflated RGB)
  worker.rs         PDF worker thread: priority queue (control > visible tiles > covers > search > prefetch)
  pdfium.rs         dlopen'ed PDFium bindings + Document wrapper (render, text, outline)
  layout.rs         page layout (single column or two-page spread rows), View transform, tile grid / zoom levels
  tiles.rs          GPU tile cache (LRU + "wanted" set, texture pool)
  search.rs         text folding + matching (pure, unit tested)
  gesture.rs        velocity tracker + Android-equivalent fling curve
  text.rs           UI text: mmapped system fonts (ab_glyph), glyph atlas
  gfx.rs            backend-neutral Gpu trait + SDF shape painter; NullGpu for tests
  gl.rs / egl.rs    OpenGL ES 3 backend and EGL context management
  ui.rs             palette + vector icons
  store.rs          last reading position + name + page count per URI, most recent first (files/positions.tsv; also the Recent tab) + settings (files/settings.txt: spread=0|1, caffeine=0|1, pin_chapters=0|1, chapters_w=<dp, 0 = auto>, shelf=<library tab: "" = Recent, else folder tree URI>, home=0|1 (library at start, default 1))
android/            Java shim (MainActivity, ReaderView), manifest, resources
examples/headless.rs  renders the real app offscreen via Mesa EGL and writes PNGs (a directory argument = library mode)
tests/pdf.rs        PDFium integration tests (need PDFIUM_PATH + JUSTPDF_TEST_PDF)
scripts/            fetch-pdfium.sh, make-test-pdf.py, package-apk.py
build.sh            Gradle-free APK build
.github/workflows/build.yml  CI: fmt + clippy + tests, APKs; every green build of main → GitHub release `build-N` (v* tags → that version)
```

## Architecture

Three threads:
1. **Java UI thread** (`MainActivity`, `ReaderView`): forwards surface/touch/key/IME/lifecycle callbacks over JNI; performs Java-only work when Rust asks (document/folder pickers, listing a library folder through SAF on a background thread, opening content URIs on a background thread → passes the fd, soft keyboard, system bars). Keep it thin — no logic in Java.
2. **Render thread** (`android.rs::run`): owns EGL + `GlGpu` + `App`. Waits on a channel (or a deadline from `App::next_deadline`), drains events, calls `App::update`, renders when `needs_redraw`, swaps (vsync-paced). Votes 120 Hz via `ANativeWindow_setFrameRate` only while animating.
3. **PDF worker** (`worker.rs`, one per pane): owns all PDFium state. The app *replaces* the whole render queue whenever the view changes, so stale requests vanish. PDFium is not thread-safe: every call holds `pdfium::LOCK`.

Panes / split view (`app.rs`): everything per document (doc, tabs, view, tiles, worker, shelf, overlay, gesture, menu, search, toast) lives in a `Pane`; `App` keeps the shared state (store, library, covers, settings, text, keyboard) plus `pane` (the focused one) and `other: Option<Box<Pane>>`. All pane code works on `self.pane`; the other pane is updated/drawn/fed by `swap_panes` → work → swap back (`with_other`, `with_pane`), and its `pane_animating`/`pane_deadline` are cached in `other_busy` when it is swapped out. A touch that starts in the other pane moves the focus (`focus_other`). Worker messages are routed by document id, `open_document` answers by URI (`openers`; a failure's message ends with the URI), picker answers to `picker_for`. There is no split button: dragging a tab (`app/tabs.rs`) onto the other half (`drop_rect`) opens it there — in a new pane if needed (`new_pane`); the tab moves, except the only tab of an unsplit window, which is shown twice. A pane whose last tab is closed or dragged away closes; Back closes the right/bottom pane and its tabs move to the pane that is left.

Tabs: `Pane::tabs`/`tab`; only the active tab's document is open (one worker document per pane). A document that opens in a pane gets a tab (`add_tab`, after the current one, or the existing tab with that URI). Tapping a tab reopens its document via `open_document` at the stored position; × closes it (the neighbour opens; the last one closes the document → library / picker, or the pane in split view). A touch on the strip becomes Press (tap), Scroll (sideways while the strip overflows) or Drag (the chip follows the finger, the target half is highlighted with "Open here", drawn over both panes in `render`). Tabs are not persisted across restarts. Panes split the window along its longer side (`slot_rect`) with a divider; each pane has its own rect (`x0`, `y0`, `w`, `vh`) and insets, draws with `Painter::ox/oy` offsets and is clipped with `Gpu::clip`; touches are converted to pane coordinates in `touch()`. Closed panes go to `retired` until the next render frees their tiles.

Rendering model:
- Document space = "base px": each row spans the viewport width at zoom 1, rows stacked with no gap (`Layout`). A row is one page, or in spread mode two pages scaled to a common height that together fill the width (a lone last page is centred at half width). Every page has its own rect (`xs`, `widths`, `tops`, `heights`); `page_at`/`range` work on rows, `page_at` returns the row's first page. `screen = (doc - view.xy) * zoom`; f64 for scroll math. Toggling spread rebuilds the layout and tiles, keeping the current page.
- The document viewport is `vw`×`vh` at pane x `ox`. `ox > 0` while the contents panel is docked (pinned + document has an outline + window wide enough to leave ≥360 dp for reading; width = `chapters_w` clamped to `dock_range`, resized by dragging the panel edge — the drag only previews, the viewport is rebuilt on release); otherwise `vw == w` (pane width). Touch x is converted to viewport coordinates in `touch()`; document/overlay painters carry `Painter::ox`, the panel draws in pane coordinates. Docking/undocking goes through `update_dock` → `set_viewport` (rebuilds the layout like a resize).
- Tiles are rendered at discrete levels `scale = 2^(level/4)` (`level_for_zoom`), max 2048 px wide (1024 when zoomed in) × 512 px tall, RGBX straight into GL textures. Plus one low-res thumbnail per page as placeholder for fast flings.
- Draw order per frame: placeholders → fallback tiles (thumbs / other levels) only where current level is incomplete → current-level tiles → search highlights → overlay UI (single SDF vertex batch: rounded rects, rings, capsules, glyphs).
- Page edges are snapped to whole pixels at 1:1 zoom; tile sampling is clamped to the valid texel area (no seams).
- Requests (`update_requests`) are *planned within the memory budget*: visible tiles (urgent, centre first) always; then one screen ahead in the scroll direction + half behind, then thumbnails ±2 screens, only while they fit in 90% of the budget. Tiles already uploaded or waiting in `uploads` (`in_flight`) are never re-requested. Eviction only drops tiles outside the wanted set (LRU) and never ones drawn this frame — evicting wanted tiles caused an endless render/evict loop (2+ GB) on device, so keep it that way. Budget ≈ 8 pane-screens of RGBA per pane (96–256 MB; 64 MB minimum each in split view).

Library (`app/shelf.rs`): shown whenever no document is open and `home` is on (so at launch; Java reports `nativeNothingToOpen` when there is no intent/restored document, and with `home` off Rust then opens the picker) and over the document from the reader's top-left button (back returns to it). Tabs: Recent (`Store::recent`) and one per added folder; a folder's books are sorted naturally and grouped under subfolder headings. Folders are tree URIs with a persisted read permission; Java walks them (depth ≤ 8, ≤ 5000 PDFs) and returns "dir\tname\turi" lines; they are rescanned at launch and when the shelf is opened after ≥ 30 s. Covers: at most 4 jobs in flight, visible cells first — from the disk cache (`CoverSrc::Disk`), else Java opens an fd (`openCover` → `nativeCoverFd`) and the worker renders page 1 (`CoverSrc::Fd`), and a document opened in the reader caches its own cover (`CoverSrc::Doc`). Cover textures live only while the shelf is shown (≤ 150, LRU); the disk cache keeps 600. While the shelf is up the document's render queue is emptied.

Search: worker folds each page's text once (lowercase, ligatures, whitespace, line-end hyphenation) and caches it; queries scan from the current page outward and stream hits per page; rects come from `FPDFText_GetRect` mapped through `FPDF_PageToDevice` to page fractions.

Java ↔ Rust contract (names must match exactly; d8 only, no R8 so nothing is renamed):
- Java → Rust: `MainActivity.native*` static methods ↔ `Java_dev_justpdf_MainActivity_native*` in `android.rs`.
- Rust → Java: `openPicker()`, `openFolderPicker()`, `scanFolder(String)`, `releaseFolder(String)`, `openDocument(String)`, `openCover(long,String)`, `showKeyboard(int,String)`, `hideKeyboard()`, `setSystemBars(boolean)`, `setKeepScreenOn(boolean)`, `exitApp()` on the activity.

## Building

Toolchain: Rust stable with Android targets, Android SDK (build-tools 36, platform 36), NDK r28, JDK 17+, python3.

```
ANDROID_HOME=... ./build.sh                    # all ABIs → build/out/justpdf-{arm64-v8a,armeabi-v7a,x86_64,universal}.apk
ABIS=arm64-v8a ./build.sh                      # faster, one ABI
```
No Gradle: cargo with the NDK clang as linker, javac + d8, aapt2, `scripts/package-apk.py` (native libs stored uncompressed, `extractNativeLibs=false`), `zipalign -P 16`, apksigner. Signing defaults to `~/.android/debug.keystore`; set `JUSTPDF_KEYSTORE`, `JUSTPDF_KEYSTORE_PASS`, `JUSTPDF_KEY_ALIAS`, `JUSTPDF_KEY_PASS` for a release key. CI uses secrets `KEYSTORE_BASE64`, `KEYSTORE_PASSWORD`, `KEY_ALIAS`, `KEY_PASSWORD` (without them each CI build gets a throwaway key).

PDFium comes prebuilt from bblanchon/pdfium-binaries, pinned by `PDFIUM_TAG` in `scripts/fetch-pdfium.sh` (into `third_party/`, gitignored).

## Testing
```
cargo test                                      # unit tests
scripts/fetch-pdfium.sh linux-x64 && python3 scripts/make-test-pdf.py /tmp/t.pdf
PDFIUM_PATH=$PWD/third_party/pdfium/linux-x64/lib/libpdfium.so JUSTPDF_TEST_PDF=/tmp/t.pdf cargo test --release
cargo clippy --all-targets -- -D warnings       # CI enforces this (also check --target aarch64-linux-android --lib)

# Visual check without a device (Mesa EGL): screenshots of the real app
PDFIUM_PATH=... cargo run --release --features headless --example headless -- /tmp/t.pdf /tmp/shots \
    shot:open drag:-2000 shot:scrolled tap:739,143 text:needle wait:800 shot:search
```
Library mode: pass a directory instead of the PDF — it becomes a library folder (fresh settings each run) and the mock platform answers scans/opens/covers from the file system; the grid starts at y≈506 (first cover centre ≈ 198,722), the Recent chip is at ≈ 124,281. On a machine without a display set `EGL_PLATFORM=surfaceless` (needs libegl-dev/libgles-dev to link).

Headless commands (window size via `JUSTPDF_HEADLESS_SIZE=WxH`, e.g. 2340x1080 for landscape): `shot:NAME wait:MS idle drag:DY dragx:DX fling:DY swipe:X,Y,DX,DY tap:X,Y dtap:X,Y pinch:X,Y,F text:S ime back key:CODE down:X,Y move:X,Y up` (1080×2340, density 2.75; top-bar buttons are at y≈143, caffeine x≈607, search x≈739, chapters x≈871, settings x≈1003 — without an outline everything left of settings moves right by 132; the tab strip is at y≈280, first chip from x≈22 (a one-letter title ≈ 22–150, "Algebra" ≈ 22–296; × at the chip's right end); drag a tab with `down:X,Y move:X,Y up` (e.g. `down:80,280 move:500,1500 up` splits a portrait window); the settings menu's rows: "Library at start" ≈ 618,330, "Two pages side by side" ≈ 618,528). In portrait the split view's second pane starts at y≈1171; in landscape (2340x1080) at x≈1172. `nothing` = launch with nothing to open, `cancel` = file picker cancelled..

## Conventions
- Keep the Java shim minimal; new behaviour goes in Rust (`app.rs`), with `Platform` trait methods for anything that needs Java.
- `App` must stay platform-independent (testable with `NullGpu` and the headless harness).
- Never call PDFium outside the worker thread / without `pdfium::LOCK`.
- Performance first: avoid per-frame allocations in hot paths, keep uploads bounded per frame (`process_uploads`), keep the render queue replace-based.
