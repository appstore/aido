use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use unicode_width::UnicodeWidthStr as _;

/// A minimal stderr spinner. Only runs when stderr is a TTY so that
/// `aido ... 2>log` stays clean.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    pub fn start(msg: &str) -> Self {
        if !std::io::stderr().is_terminal() {
            return Self::disabled();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let msg = msg.to_string();
        let handle = std::thread::spawn(move || {
            const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut i = 0usize;
            let mut err = std::io::stderr().lock();
            while !flag.load(Ordering::Relaxed) {
                let _ = write!(err, "\r{} {}", FRAMES[i % FRAMES.len()], msg);
                let _ = err.flush();
                i += 1;
                std::thread::sleep(Duration::from_millis(80));
            }
            // +2 covers the frame glyph and the space after it; display width
            // rather than char count so CJK model names erase cleanly.
            let _ = write!(err, "\r{}\r", " ".repeat(msg.width() + 2));
            let _ = err.flush();
        });
        Self { stop, handle: Some(handle) }
    }

    pub fn disabled() -> Self {
        Self { stop: Arc::new(AtomicBool::new(true)), handle: None }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
