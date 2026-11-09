# JustPDF

Minimal, fast Android PDF reader written in Rust: continuous vertical (webtoon) scrolling, optional two-page spreads, chapters (PDF outline), full-text search. No home screen: launch → system file picker → viewer. Also opens PDFs via "Open with" / "Share".

## Layout of the repo
```
src/                Rust crate (cdylib libjustpdf.so + rlib for tests)
  android.rs        JNI exports + render thread (EGL, frame loop, Java callbacks)
  app.rs            platform-independent state machine: input, view, tile requests, overlay UI
  worker.rs         PDF worker thread: priority queue (control > visible tiles > search > prefetch)
  pdfium.rs         dlopen'ed PDFium bindings + Document wrapper (render, text, outline)
  layout.rs         page layout (single column or two-page spread rows), View transform, tile grid / zoom levels
  tiles.rs          GPU tile cache (LRU + "wanted" set, texture pool)
  search.rs         text folding + matching (pure, unit tested)
  gesture.rs        velocity tracker + Android-equivalent fling curve
  text.rs           UI text: mmapped system fonts (ab_glyph), glyph atlas
  gfx.rs            backend-neutral Gpu trait + SDF shape painter; NullGpu for tests
  gl.rs / egl.rs    OpenGL ES 3 backend and EGL context management
  ui.rs             palette + vector icons
  store.rs          last reading position per URI (files/positions.tsv) + settings (files/settings.txt: spread=0|1, caffeine=0|1)
android/            Java shim (MainActivity, ReaderView), manifest, resources
examples/headless.rs  renders the real app offscreen via Mesa EGL and writes PNGs
tests/pdf.rs        PDFium integration tests (need PDFIUM_PATH + JUSTPDF_TEST_PDF)
scripts/            fetch-pdfium.sh, make-test-pdf.py, package-apk.py
build.sh            Gradle-free APK build
.github/workflows/build.yml  CI: fmt + clippy + tests, APKs; every green build of main → GitHub release `build-N` (v* tags → that version)
```

## Architecture

Three threads:
1. **Java UI thread** (`MainActivity`, `ReaderView`): forwards surface/touch/key/IME/lifecycle callbacks over JNI; performs Java-only work when Rust asks (document picker, opening content URIs on a background thread → passes the fd, soft keyboard, system bars). Keep it thin — no logic in Java.
2. **Render thread** (`android.rs::run`): owns EGL + `GlGpu` + `App`. Waits on a channel (or a deadline from `App::next_deadline`), drains events, calls `App::update`, renders when `needs_redraw`, swaps (vsync-paced). Votes 120 Hz via `ANativeWindow_setFrameRate` only while animating.
3. **PDF worker** (`worker.rs`): owns all PDFium state. The app *replaces* the whole render queue whenever the view changes, so stale requests vanish. PDFium is not thread-safe: every call holds `pdfium::LOCK`.

Rendering model:
- Document space = "base px": each row spans the viewport width at zoom 1, rows stacked with no gap (`Layout`). A row is one page, or in spread mode two pages scaled to a common height that together fill the width (a lone last page is centred at half width). Every page has its own rect (`xs`, `widths`, `tops`, `heights`); `page_at`/`range` work on rows, `page_at` returns the row's first page. `screen = (doc - view.xy) * zoom`; f64 for scroll math. Toggling spread rebuilds the layout and tiles, keeping the current page.
- Tiles are rendered at discrete levels `scale = 2^(level/4)` (`level_for_zoom`), max 2048 px wide (1024 when zoomed in) × 512 px tall, RGBX straight into GL textures. Plus one low-res thumbnail per page as placeholder for fast flings.
- Draw order per frame: placeholders → fallback tiles (thumbs / other levels) only where current level is incomplete → current-level tiles → search highlights → overlay UI (single SDF vertex batch: rounded rects, rings, capsules, glyphs).
- Page edges are snapped to whole pixels at 1:1 zoom; tile sampling is clamped to the valid texel area (no seams).
- Requests (`update_requests`) are *planned within the memory budget*: visible tiles (urgent, centre first) always; then one screen ahead in the scroll direction + half behind, then thumbnails ±2 screens, only while they fit in 90% of the budget. Tiles already uploaded or waiting in `uploads` (`in_flight`) are never re-requested. Eviction only drops tiles outside the wanted set (LRU) and never ones drawn this frame — evicting wanted tiles caused an endless render/evict loop (2+ GB) on device, so keep it that way. Budget ≈ 8 screens of RGBA (96–256 MB).

Search: worker folds each page's text once (lowercase, ligatures, whitespace, line-end hyphenation) and caches it; queries scan from the current page outward and stream hits per page; rects come from `FPDFText_GetRect` mapped through `FPDF_PageToDevice` to page fractions.

Java ↔ Rust contract (names must match exactly; d8 only, no R8 so nothing is renamed):
- Java → Rust: `MainActivity.native*` static methods ↔ `Java_dev_justpdf_MainActivity_native*` in `android.rs`.
- Rust → Java: `openPicker()`, `showKeyboard(int,String)`, `hideKeyboard()`, `setSystemBars(boolean)`, `setKeepScreenOn(boolean)`, `exitApp()` on the activity.

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
    shot:open drag:-2000 shot:scrolled tap:871,143 text:needle wait:800 shot:search
```
Headless commands: `shot:NAME wait:MS idle drag:DY dragx:DX fling:DY tap:X,Y dtap:X,Y pinch:X,Y,F text:S ime back key:CODE` (1080×2340, density 2.75; top-bar buttons are at y≈143, caffeine x≈607, spread x≈739, search x≈871, chapters x≈1003).

## Conventions
- Keep the Java shim minimal; new behaviour goes in Rust (`app.rs`), with `Platform` trait methods for anything that needs Java.
- `App` must stay platform-independent (testable with `NullGpu` and the headless harness).
- Never call PDFium outside the worker thread / without `pdfium::LOCK`.
- Performance first: avoid per-frame allocations in hot paths, keep uploads bounded per frame (`process_uploads`), keep the render queue replace-based.
