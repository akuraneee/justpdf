package dev.justpdf;

import android.content.Context;
import android.text.Editable;
import android.text.InputType;
import android.text.Selection;
import android.view.InputDevice;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.SurfaceView;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;

/**
 * The single view: a SurfaceView that Rust renders into with GLES. It forwards
 * touch, wheel and key events and acts as the text editor for the soft
 * keyboard (the editable text is mirrored to Rust after every change).
 */
final class ReaderView extends SurfaceView {
    static final int IME_NONE = 0;
    static final int IME_TEXT = 1;
    static final int IME_NUMBER = 2;
    static final int IME_PASSWORD = 3;

    private final MainActivity activity;
    private final InputMethodManager imm;
    private final Editable text = Editable.Factory.getInstance().newEditable("");
    private int imeMode = IME_NONE;
    private int[] ids = new int[10];
    private float[] xy = new float[20];

    ReaderView(MainActivity activity) {
        super(activity);
        this.activity = activity;
        this.imm = (InputMethodManager) activity.getSystemService(Context.INPUT_METHOD_SERVICE);
        setFocusable(true);
        setFocusableInTouchMode(true);
    }

    @Override
    public boolean onTouchEvent(MotionEvent e) {
        int n = e.getPointerCount();
        if (ids.length < n) {
            ids = new int[n];
            xy = new float[n * 2];
        }
        for (int i = 0; i < n; i++) {
            ids[i] = e.getPointerId(i);
            xy[2 * i] = e.getX(i);
            xy[2 * i + 1] = e.getY(i);
        }
        MainActivity.nativeTouch(activity.handle(), e.getActionMasked(), e.getActionIndex(), n, ids, xy, e.getEventTime());
        return true;
    }

    @Override
    public boolean onGenericMotionEvent(MotionEvent e) {
        if (e.isFromSource(InputDevice.SOURCE_CLASS_POINTER) && e.getActionMasked() == MotionEvent.ACTION_SCROLL) {
            MainActivity.nativeWheel(activity.handle(), e.getX(), e.getY(), e.getAxisValue(MotionEvent.AXIS_VSCROLL));
            return true;
        }
        return super.onGenericMotionEvent(e);
    }

    // ----- keyboard --------------------------------------------------------------------

    @Override
    public boolean onCheckIsTextEditor() {
        return imeMode != IME_NONE;
    }

    @Override
    public InputConnection onCreateInputConnection(EditorInfo info) {
        if (imeMode == IME_NONE) {
            return null;
        }
        switch (imeMode) {
            case IME_NUMBER:
                info.inputType = InputType.TYPE_CLASS_NUMBER;
                info.imeOptions = EditorInfo.IME_ACTION_GO;
                break;
            case IME_PASSWORD:
                info.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_PASSWORD;
                info.imeOptions = EditorInfo.IME_ACTION_DONE;
                break;
            default:
                info.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS;
                info.imeOptions = EditorInfo.IME_ACTION_SEARCH;
                break;
        }
        info.imeOptions |= EditorInfo.IME_FLAG_NO_EXTRACT_UI | EditorInfo.IME_FLAG_NO_FULLSCREEN;
        info.initialSelStart = Selection.getSelectionStart(text);
        info.initialSelEnd = Selection.getSelectionEnd(text);
        return new Connection();
    }

    private final class Connection extends BaseInputConnection {
        Connection() {
            super(ReaderView.this, true);
        }

        @Override
        public Editable getEditable() {
            return text;
        }

        @Override
        public boolean commitText(CharSequence t, int newCursorPosition) {
            boolean r = super.commitText(t, newCursorPosition);
            sync();
            return r;
        }

        @Override
        public boolean setComposingText(CharSequence t, int newCursorPosition) {
            boolean r = super.setComposingText(t, newCursorPosition);
            sync();
            return r;
        }

        @Override
        public boolean finishComposingText() {
            boolean r = super.finishComposingText();
            sync();
            return r;
        }

        @Override
        public boolean deleteSurroundingText(int before, int after) {
            boolean r = super.deleteSurroundingText(before, after);
            sync();
            return r;
        }

        @Override
        public boolean deleteSurroundingTextInCodePoints(int before, int after) {
            boolean r = super.deleteSurroundingTextInCodePoints(before, after);
            sync();
            return r;
        }

        @Override
        public boolean setSelection(int start, int end) {
            boolean r = super.setSelection(start, end);
            sync();
            return r;
        }

        @Override
        public boolean performEditorAction(int actionCode) {
            MainActivity.nativeImeAction(activity.handle());
            return true;
        }
    }

    private void sync() {
        MainActivity.nativeText(activity.handle(), text.toString(), Math.max(0, Selection.getSelectionEnd(text)));
    }

    @Override
    public boolean onKeyDown(int keyCode, KeyEvent event) {
        if (imeMode != IME_NONE) {
            int start = Math.max(0, Math.min(Selection.getSelectionStart(text), Selection.getSelectionEnd(text)));
            int end = Math.max(0, Math.max(Selection.getSelectionStart(text), Selection.getSelectionEnd(text)));
            if (keyCode == KeyEvent.KEYCODE_DEL) {
                if (start != end) {
                    text.delete(start, end);
                } else if (start > 0) {
                    int prev = Character.offsetByCodePoints(text, start, -1);
                    text.delete(prev, start);
                }
                sync();
                return true;
            }
            if (keyCode == KeyEvent.KEYCODE_ENTER || keyCode == KeyEvent.KEYCODE_NUMPAD_ENTER) {
                MainActivity.nativeImeAction(activity.handle());
                return true;
            }
            int c = event.getUnicodeChar();
            if (c != 0 && !Character.isISOControl(c)) {
                text.replace(start, end, new String(Character.toChars(c)));
                sync();
                return true;
            }
        } else if (activity.onScrollKey(keyCode)) {
            return true;
        }
        return super.onKeyDown(keyCode, event);
    }

    void showIme(int mode, String initial) {
        imeMode = mode;
        text.replace(0, text.length(), initial);
        Selection.setSelection(text, text.length());
        requestFocus();
        imm.restartInput(this);
        if (!imm.showSoftInput(this, 0)) {
            // The window may not have focus yet (e.g. right after a picker); retry shortly.
            postDelayed(() -> {
                if (imeMode != IME_NONE) {
                    requestFocus();
                    imm.showSoftInput(this, 0);
                }
            }, 150);
        }
    }

    void hideIme() {
        if (imeMode == IME_NONE) {
            return;
        }
        imeMode = IME_NONE;
        imm.hideSoftInputFromWindow(getWindowToken(), 0);
        imm.restartInput(this);
    }
}
