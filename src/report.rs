//! Progress lines go to stderr and to an in-memory buffer the web page polls.

use std::sync::Mutex;

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn say(line: String) {
    eprintln!("{line}");
    LINES.lock().unwrap().push(line);
}

pub fn clear() {
    LINES.lock().unwrap().clear();
}

pub fn lines() -> Vec<String> {
    LINES.lock().unwrap().clone()
}

#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => { $crate::report::say(format!($($arg)*)) };
}
