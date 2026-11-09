//! Android input and IME codes <-> app types. The conversion happens here, at
//! the platform boundary, so `app` never sees Android constants. Kept out of
//! `android.rs` (JNI, Android-only) so it is unit tested on any host and the
//! headless harness can take the same key codes.

use crate::app::{Key, Keyboard, TouchPhase};

/// `MotionEvent.getActionMasked()` -> phase. Other actions (hover, outside, ...) are dropped.
pub fn touch_phase(action: i32) -> Option<TouchPhase> {
    Some(match action {
        0 => TouchPhase::Down,        // ACTION_DOWN
        1 => TouchPhase::Up,          // ACTION_UP
        2 => TouchPhase::Move,        // ACTION_MOVE
        3 => TouchPhase::Cancel,      // ACTION_CANCEL
        5 => TouchPhase::PointerDown, // ACTION_POINTER_DOWN
        6 => TouchPhase::PointerUp,   // ACTION_POINTER_UP
        _ => return None,
    })
}

/// `KeyEvent` key code -> navigation key (the codes `MainActivity.onScrollKey` forwards).
pub fn key(code: i32) -> Option<Key> {
    Some(match code {
        19 => Key::Up,            // KEYCODE_DPAD_UP
        20 => Key::Down,          // KEYCODE_DPAD_DOWN
        21 => Key::Left,          // KEYCODE_DPAD_LEFT
        22 => Key::Right,         // KEYCODE_DPAD_RIGHT
        92 => Key::PageUp,        // KEYCODE_PAGE_UP
        93 | 62 => Key::PageDown, // KEYCODE_PAGE_DOWN, KEYCODE_SPACE
        122 => Key::Home,         // KEYCODE_MOVE_HOME
        123 => Key::End,          // KEYCODE_MOVE_END
        _ => return None,
    })
}

/// Mode argument of `MainActivity.showKeyboard(int, String)` (`ReaderView.IME_*`).
pub fn ime_mode(kb: Keyboard) -> i32 {
    match kb {
        Keyboard::Text => 1,
        Keyboard::Number => 2,
        Keyboard::Password => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_actions() {
        assert_eq!(touch_phase(0), Some(TouchPhase::Down));
        assert_eq!(touch_phase(1), Some(TouchPhase::Up));
        assert_eq!(touch_phase(2), Some(TouchPhase::Move));
        assert_eq!(touch_phase(3), Some(TouchPhase::Cancel));
        assert_eq!(touch_phase(5), Some(TouchPhase::PointerDown));
        assert_eq!(touch_phase(6), Some(TouchPhase::PointerUp));
        assert_eq!(touch_phase(4), None); // ACTION_OUTSIDE
        assert_eq!(touch_phase(7), None); // ACTION_HOVER_MOVE
    }

    #[test]
    fn key_codes() {
        assert_eq!(key(62), Some(Key::PageDown));
        assert_eq!(key(93), Some(Key::PageDown));
        assert_eq!(key(122), Some(Key::Home));
        assert_eq!(key(66), None); // KEYCODE_ENTER
    }

    #[test]
    fn ime_modes() {
        assert_eq!(ime_mode(Keyboard::Text), 1);
        assert_eq!(ime_mode(Keyboard::Number), 2);
        assert_eq!(ime_mode(Keyboard::Password), 3);
    }
}
