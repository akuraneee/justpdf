package dev.justpdf;

import android.app.Activity;
import android.content.ActivityNotFoundException;
import android.content.Intent;
import android.database.Cursor;
import android.graphics.Insets;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.ParcelFileDescriptor;
import android.provider.DocumentsContract;
import android.provider.DocumentsContract.Document;
import android.provider.OpenableColumns;
import android.view.DisplayCutout;
import android.view.KeyEvent;
import android.view.SurfaceHolder;
import android.view.View;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;
import android.window.OnBackInvokedDispatcher;

import java.util.ArrayList;
import java.util.Locale;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;

/**
 * Thin platform shim. Everything (rendering, input handling, UI, PDF) lives in
 * the Rust library; this class only forwards Android callbacks and performs the
 * few operations that require Java APIs: the document and folder pickers,
 * listing library folders, opening content URIs, the soft keyboard, system bar
 * visibility and keeping the screen on.
 */
public final class MainActivity extends Activity implements SurfaceHolder.Callback {
    static {
        System.loadLibrary("pdfium");
        System.loadLibrary("justpdf");
    }

    private static final int REQ_OPEN = 1;
    private static final int REQ_FOLDER = 2;
    private static final String STATE_URI = "uri";
    /** Folder scans: how deep and how many PDFs at most. */
    private static final int SCAN_DEPTH = 8;
    private static final int SCAN_MAX = 5000;

    private final Object lock = new Object();
    private volatile long handle;
    private ReaderView view;
    private boolean pickerOpen;
    /** Folder scans, one at a time. */
    private final ExecutorService scanner = Executors.newSingleThreadExecutor();
    /** Opens documents for their covers (providers may be slow). */
    private final ExecutorService coverOpener = Executors.newFixedThreadPool(2);

    // ----- native interface ------------------------------------------------------------

    static native long nativeCreate(MainActivity activity, String filesDir, float density);
    static native void nativeDestroy(long h);
    static native void nativeSurface(long h, Object surface);
    static native void nativeSurfaceSize(long h, int width, int height);
    static native void nativeInsets(long h, int left, int top, int right, int bottom);
    static native void nativeTouch(long h, int action, int index, int count, int[] ids, float[] xy, long timeMs);
    static native void nativeWheel(long h, float x, float y, float delta);
    static native void nativeKey(long h, int keyCode);
    static native boolean nativeBack(long h);
    static native void nativeOpen(long h, int fd, String uri, String name);
    static native void nativeOpenFailed(long h, String message);
    static native void nativePickerCancelled(long h);
    static native void nativeNothingToOpen(long h);
    static native void nativeFolderPicked(long h, String uri, String name);
    static native void nativeFolderScanned(long h, String uri, String list, boolean ok);
    static native void nativeCoverFd(long h, long key, int fd);
    static native void nativeText(long h, String text, int cursor);
    static native void nativeImeAction(long h);
    static native void nativePause(long h);
    static native String nativeCurrentUri(long h);

    long handle() {
        return handle;
    }

    // ----- lifecycle -------------------------------------------------------------------

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        Window w = getWindow();
        w.addFlags(WindowManager.LayoutParams.FLAG_DRAWS_SYSTEM_BAR_BACKGROUNDS);
        w.setStatusBarColor(0x00000000);
        w.setNavigationBarColor(0x00000000);
        if (Build.VERSION.SDK_INT >= 28) {
            WindowManager.LayoutParams lp = w.getAttributes();
            lp.layoutInDisplayCutoutMode = WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
            w.setAttributes(lp);
        }
        if (Build.VERSION.SDK_INT >= 29) {
            w.setNavigationBarContrastEnforced(false);
        }

        view = new ReaderView(this);
        view.getHolder().addCallback(this);
        setContentView(view);
        if (Build.VERSION.SDK_INT >= 30) {
            w.setDecorFitsSystemWindows(false);
        } else {
            view.setSystemUiVisibility(View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                    | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                    | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION);
        }
        view.setOnApplyWindowInsetsListener((v, insets) -> {
            sendInsets(insets);
            return insets;
        });

        if (Build.VERSION.SDK_INT >= 33) {
            // targetSdk 36: onBackPressed() is no longer dispatched on Android 16+.
            getOnBackInvokedDispatcher().registerOnBackInvokedCallback(
                    OnBackInvokedDispatcher.PRIORITY_DEFAULT, this::onBackPressed);
        }

        handle = nativeCreate(this, getFilesDir().getAbsolutePath(), getResources().getDisplayMetrics().density);

        // Without a document to open, Rust shows the library (or the file
        // picker, if the library is turned off).
        String saved = state != null ? state.getString(STATE_URI) : null;
        if (saved != null) {
            openUri(Uri.parse(saved), true);
        } else if (!handleIntent(getIntent())) {
            nativeNothingToOpen(handle);
        }
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        handleIntent(intent);
    }

    @Override
    protected void onPause() {
        super.onPause();
        nativePause(handle);
    }

    @Override
    protected void onSaveInstanceState(Bundle out) {
        super.onSaveInstanceState(out);
        String uri = nativeCurrentUri(handle);
        if (uri != null) {
            out.putString(STATE_URI, uri);
        }
    }

    @Override
    protected void onDestroy() {
        long h;
        synchronized (lock) {
            h = handle;
            handle = 0;
        }
        nativeDestroy(h);
        scanner.shutdownNow();
        coverOpener.shutdownNow();
        super.onDestroy();
    }

    @Override
    @SuppressWarnings("deprecation")
    public void onBackPressed() {
        if (!nativeBack(handle)) {
            super.onBackPressed();
        }
    }

    // ----- surface ---------------------------------------------------------------------

    @Override
    public void surfaceCreated(SurfaceHolder holder) {
        nativeSurface(handle, holder.getSurface());
    }

    @Override
    public void surfaceChanged(SurfaceHolder holder, int format, int width, int height) {
        nativeSurfaceSize(handle, width, height);
    }

    @Override
    public void surfaceDestroyed(SurfaceHolder holder) {
        nativeSurface(handle, null);
    }

    @SuppressWarnings("deprecation")
    private void sendInsets(WindowInsets insets) {
        int l, t, r, b;
        // Stable insets (as if the bars were shown) so the overlay does not jump
        // when the system bars are toggled together with it.
        if (Build.VERSION.SDK_INT >= 30) {
            Insets i = insets.getInsetsIgnoringVisibility(WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
            l = i.left; t = i.top; r = i.right; b = i.bottom;
        } else {
            l = insets.getStableInsetLeft();
            t = insets.getStableInsetTop();
            r = insets.getStableInsetRight();
            b = insets.getStableInsetBottom();
            if (Build.VERSION.SDK_INT >= 28) {
                DisplayCutout c = insets.getDisplayCutout();
                if (c != null) {
                    l = Math.max(l, c.getSafeInsetLeft());
                    t = Math.max(t, c.getSafeInsetTop());
                    r = Math.max(r, c.getSafeInsetRight());
                    b = Math.max(b, c.getSafeInsetBottom());
                }
            }
        }
        nativeInsets(handle, l, t, r, b);
    }

    // ----- input from ReaderView -------------------------------------------------------

    boolean onScrollKey(int keyCode) {
        switch (keyCode) {
            case KeyEvent.KEYCODE_DPAD_UP:
            case KeyEvent.KEYCODE_DPAD_DOWN:
            case KeyEvent.KEYCODE_DPAD_LEFT:
            case KeyEvent.KEYCODE_DPAD_RIGHT:
            case KeyEvent.KEYCODE_PAGE_UP:
            case KeyEvent.KEYCODE_PAGE_DOWN:
            case KeyEvent.KEYCODE_SPACE:
            case KeyEvent.KEYCODE_MOVE_HOME:
            case KeyEvent.KEYCODE_MOVE_END:
                nativeKey(handle, keyCode);
                return true;
            default:
                return false;
        }
    }

    // ----- documents -------------------------------------------------------------------

    private boolean handleIntent(Intent intent) {
        if (intent == null) {
            return false;
        }
        Uri uri = null;
        if (Intent.ACTION_VIEW.equals(intent.getAction())) {
            uri = intent.getData();
        } else if (Intent.ACTION_SEND.equals(intent.getAction())) {
            uri = intent.getParcelableExtra(Intent.EXTRA_STREAM);
        }
        if (uri == null) {
            return false;
        }
        openUri(uri, false);
        return true;
    }

    /** Called from Rust (render thread). */
    void openPicker() {
        runOnUiThread(() -> {
            if (pickerOpen || isFinishing()) {
                return;
            }
            Intent i = new Intent(Intent.ACTION_OPEN_DOCUMENT);
            i.addCategory(Intent.CATEGORY_OPENABLE);
            i.setType("application/pdf");
            try {
                startActivityForResult(i, REQ_OPEN);
                pickerOpen = true;
            } catch (ActivityNotFoundException e) {
                Intent g = new Intent(Intent.ACTION_GET_CONTENT);
                g.addCategory(Intent.CATEGORY_OPENABLE);
                g.setType("application/pdf");
                try {
                    startActivityForResult(g, REQ_OPEN);
                    pickerOpen = true;
                } catch (ActivityNotFoundException e2) {
                    nativeOpenFailed(handle, "No file picker available");
                }
            }
        });
    }

    /** Called from Rust (render thread). */
    void openFolderPicker() {
        runOnUiThread(() -> {
            if (pickerOpen || isFinishing()) {
                return;
            }
            Intent i = new Intent(Intent.ACTION_OPEN_DOCUMENT_TREE);
            i.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION | Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
            try {
                startActivityForResult(i, REQ_FOLDER);
                pickerOpen = true;
            } catch (ActivityNotFoundException e) {
                // No folder picker on this device: nothing to do.
            }
        });
    }

    /** Called from Rust: open a document from the library. */
    void openDocument(String uri) {
        openUri(Uri.parse(uri), false);
    }

    /** Called from Rust: open a document read-only so its first page can become a cover. */
    void openCover(final long key, final String uri) {
        coverOpener.execute(() -> {
            int fd = -1;
            try {
                ParcelFileDescriptor pfd = getContentResolver().openFileDescriptor(Uri.parse(uri), "r");
                if (pfd != null) {
                    fd = pfd.detachFd();
                }
            } catch (Exception e) {
                fd = -1;
            }
            synchronized (lock) {
                if (handle != 0) {
                    nativeCoverFd(handle, key, fd);
                } else if (fd >= 0) {
                    closeQuietly(ParcelFileDescriptor.adoptFd(fd));
                }
            }
        });
    }

    /** Called from Rust: the folder left the library. */
    void releaseFolder(String uri) {
        try {
            getContentResolver().releasePersistableUriPermission(Uri.parse(uri), Intent.FLAG_GRANT_READ_URI_PERMISSION);
        } catch (Exception ignored) {
            // Already gone.
        }
    }

    /**
     * Called from Rust: lists the PDFs under a library folder (tree URI) and
     * hands them back as "folder path \t name \t document URI" lines.
     */
    void scanFolder(final String treeUri) {
        scanner.execute(() -> {
            StringBuilder out = new StringBuilder();
            boolean ok;
            try {
                Uri tree = Uri.parse(treeUri);
                walk(tree, DocumentsContract.getTreeDocumentId(tree), "", 0, out, new int[]{0});
                ok = true;
            } catch (Exception e) {
                ok = false;
            }
            synchronized (lock) {
                if (handle != 0) {
                    nativeFolderScanned(handle, treeUri, out.toString(), ok);
                }
            }
        });
    }

    private void walk(Uri tree, String docId, String path, int depth, StringBuilder out, int[] count) {
        Uri children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, docId);
        String[] cols = {Document.COLUMN_DOCUMENT_ID, Document.COLUMN_DISPLAY_NAME, Document.COLUMN_MIME_TYPE};
        ArrayList<String[]> dirs = new ArrayList<>();
        try (Cursor c = getContentResolver().query(children, cols, null, null, null)) {
            if (c == null) {
                throw new IllegalStateException("cannot list " + children);
            }
            while (c.moveToNext()) {
                String id = c.getString(0);
                String name = c.getString(1);
                String mime = c.getString(2);
                if (id == null || name == null) {
                    continue;
                }
                name = name.replace('\t', ' ').replace('\n', ' ');
                if (Document.MIME_TYPE_DIR.equals(mime)) {
                    if (depth < SCAN_DEPTH && !name.startsWith(".")) {
                        dirs.add(new String[]{id, name});
                    }
                } else if (count[0] < SCAN_MAX
                        && ("application/pdf".equals(mime) || name.toLowerCase(Locale.ROOT).endsWith(".pdf"))) {
                    out.append(path).append('\t').append(name).append('\t')
                            .append(DocumentsContract.buildDocumentUriUsingTree(tree, id)).append('\n');
                    count[0]++;
                }
            }
        }
        for (String[] d : dirs) {
            try {
                walk(tree, d[0], path.isEmpty() ? d[1] : path + "/" + d[1], depth + 1, out, count);
            } catch (Exception ignored) {
                // Skip unreadable subfolders.
            }
        }
    }

    private String folderName(Uri tree) {
        try {
            Uri doc = DocumentsContract.buildDocumentUriUsingTree(tree, DocumentsContract.getTreeDocumentId(tree));
            try (Cursor c = getContentResolver().query(doc, new String[]{Document.COLUMN_DISPLAY_NAME}, null, null, null)) {
                if (c != null && c.moveToFirst() && !c.isNull(0)) {
                    return c.getString(0);
                }
            }
        } catch (Exception ignored) {
            // Fall through to the path segment.
        }
        String seg = tree.getLastPathSegment();
        return seg != null ? seg.substring(seg.lastIndexOf(':') + 1) : "Folder";
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request == REQ_FOLDER) {
            pickerOpen = false;
            final Uri tree = (result == RESULT_OK && data != null) ? data.getData() : null;
            if (tree == null) {
                return;
            }
            try {
                getContentResolver().takePersistableUriPermission(tree, Intent.FLAG_GRANT_READ_URI_PERMISSION);
            } catch (SecurityException ignored) {
                // Not persistable: usable for this session only.
            }
            scanner.execute(() -> {
                String name = folderName(tree);
                synchronized (lock) {
                    if (handle != 0) {
                        nativeFolderPicked(handle, tree.toString(), name);
                    }
                }
            });
            return;
        }
        if (request != REQ_OPEN) {
            return;
        }
        pickerOpen = false;
        Uri uri = (result == RESULT_OK && data != null) ? data.getData() : null;
        if (uri == null) {
            nativePickerCancelled(handle);
            return;
        }
        try {
            getContentResolver().takePersistableUriPermission(uri, Intent.FLAG_GRANT_READ_URI_PERMISSION);
        } catch (SecurityException ignored) {
            // Not persistable (e.g. GET_CONTENT); fine for this session.
        }
        openUri(uri, false);
    }

    /** Opens the URI off the UI thread (providers may download) and hands the fd to Rust. */
    private void openUri(final Uri uri, final boolean restoring) {
        new Thread(() -> {
            ParcelFileDescriptor pfd = null;
            String name = null;
            try {
                pfd = getContentResolver().openFileDescriptor(uri, "r");
                name = displayName(uri);
            } catch (Exception e) {
                pfd = null;
            }
            synchronized (lock) {
                if (handle == 0) {
                    closeQuietly(pfd);
                    return;
                }
                if (pfd == null) {
                    // A document from the last session that is gone: start as if fresh.
                    if (restoring) {
                        nativeNothingToOpen(handle);
                    } else {
                        nativeOpenFailed(handle, "Cannot open " + uri);
                    }
                    return;
                }
                nativeOpen(handle, pfd.detachFd(), uri.toString(), name != null ? name : "Document");
            }
        }, "open-uri").start();
    }

    private String displayName(Uri uri) {
        if ("file".equals(uri.getScheme())) {
            return uri.getLastPathSegment();
        }
        try (Cursor c = getContentResolver().query(uri, new String[]{OpenableColumns.DISPLAY_NAME}, null, null, null)) {
            if (c != null && c.moveToFirst() && !c.isNull(0)) {
                return c.getString(0);
            }
        } catch (Exception ignored) {
            // Fall through to the path segment.
        }
        return uri.getLastPathSegment();
    }

    private static void closeQuietly(ParcelFileDescriptor pfd) {
        if (pfd != null) {
            try {
                pfd.close();
            } catch (Exception ignored) {
                // Nothing to do.
            }
        }
    }

    // ----- called from Rust ------------------------------------------------------------

    void showKeyboard(final int mode, final String text) {
        runOnUiThread(() -> view.showIme(mode, text));
    }

    void hideKeyboard() {
        runOnUiThread(() -> view.hideIme());
    }

    @SuppressWarnings("deprecation")
    void setSystemBars(final boolean visible) {
        runOnUiThread(() -> {
            if (Build.VERSION.SDK_INT >= 30) {
                WindowInsetsController c = getWindow().getInsetsController();
                if (c == null) {
                    return;
                }
                c.setSystemBarsBehavior(WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
                if (visible) {
                    c.show(WindowInsets.Type.systemBars());
                } else {
                    c.hide(WindowInsets.Type.systemBars());
                }
            } else {
                int flags = View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                        | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION;
                if (!visible) {
                    flags |= View.SYSTEM_UI_FLAG_FULLSCREEN
                            | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                            | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY;
                }
                view.setSystemUiVisibility(flags);
            }
        });
    }

    void setKeepScreenOn(final boolean on) {
        runOnUiThread(() -> {
            if (on) {
                getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            } else {
                getWindow().clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
            }
        });
    }

    void exitApp() {
        runOnUiThread(this::finish);
    }
}
