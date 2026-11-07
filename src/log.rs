//! Minimal logging: logcat on Android, stderr elsewhere.

pub const INFO: i32 = 4;
pub const WARN: i32 = 5;
pub const ERROR: i32 = 6;

#[cfg(target_os = "android")]
#[link(name = "log")]
extern "C" {
    fn __android_log_write(prio: i32, tag: *const std::ffi::c_char, text: *const std::ffi::c_char) -> i32;
}

pub fn write(level: i32, msg: &str) {
    #[cfg(target_os = "android")]
    {
        let text = std::ffi::CString::new(msg.replace('\0', " ")).unwrap_or_default();
        unsafe { __android_log_write(level, c"justpdf".as_ptr(), text.as_ptr()) };
    }
    #[cfg(not(target_os = "android"))]
    {
        let tag = match level {
            ERROR => "E",
            WARN => "W",
            _ => "I",
        };
        eprintln!("[justpdf {tag}] {msg}");
    }
}

#[macro_export]
macro_rules! log_i { ($($t:tt)*) => { $crate::log::write($crate::log::INFO, &format!($($t)*)) } }
#[macro_export]
macro_rules! log_w { ($($t:tt)*) => { $crate::log::write($crate::log::WARN, &format!($($t)*)) } }
#[macro_export]
macro_rules! log_e { ($($t:tt)*) => { $crate::log::write($crate::log::ERROR, &format!($($t)*)) } }
