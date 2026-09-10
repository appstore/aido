use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use unicode_width::UnicodeWidthStr as _;

/// A minimal stderr spinner. Only runs when stderr is a TTY so that
/// `aido ... 2>log` stays clean.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    progress: Arc<AtomicU64>,
    message: Arc<std::sync::Mutex<String>>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    pub fn start(msg: &str) -> Self {
        if !std::io::stderr().is_terminal() {
            return Self::disabled();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let message = Arc::new(std::sync::Mutex::new(msg.to_string()));
        let flag = stop.clone();
        let counter = progress.clone();
        let shared_msg = message.clone();
        let handle = std::thread::spawn(move || {
            const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let mut i = 0usize;
            let mut err = std::io::stderr().lock();
            // The line grows with the live count, so each frame must erase
            // what the previous one printed instead of overwriting in place.
            let mut last_width = 0usize;
            while !flag.load(Ordering::Relaxed) {
                let msg = shared_msg.lock().map(|m| m.clone()).unwrap_or_default();
                let line = spinner_line(
                    FRAMES[i % FRAMES.len()],
                    &msg,
                    counter.load(Ordering::Relaxed),
                );
                let _ = write!(err, "\r{}\r{}", " ".repeat(last_width), line);
                let _ = err.flush();
                // Display width rather than char count so CJK model names
                // and the braille frame erase cleanly.
                last_width = line.width();
                i += 1;
                std::thread::sleep(Duration::from_millis(80));
            }
            let _ = write!(err, "\r{}\r", " ".repeat(last_width));
            let _ = err.flush();
        });
        Self {
            stop,
            progress,
            message,
            handle: Some(handle),
        }
    }

    pub fn disabled() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(true)),
            progress: Arc::new(AtomicU64::new(0)),
            message: Arc::new(std::sync::Mutex::new(String::new())),
            handle: None,
        }
    }

    /// Update the live character count shown after the message; 0 keeps the
    /// bare message, so reasoning-only phases (no content deltas yet) don't
    /// look like a stuck zero.
    pub fn set_progress(&self, chars: u64) {
        self.progress.store(chars, Ordering::Relaxed);
    }

    /// Replace the message line (per-request labels during multi-request runs).
    pub fn set_message(&self, msg: &str) {
        if let Ok(mut message) = self.message.lock() {
            *message = msg.to_string();
        }
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        // Cancellation paths (Ctrl+C dropping the run future) discard the
        // spinner without calling stop(): signal the thread so it ends
        // within one tick instead of spinning against a dying process.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// One rendered spinner line: the frame, the message, and — once content
/// has started arriving — the live count, e.g. `⠙ asking glm-4.6... 1,204 chars`.
fn spinner_line(frame: char, msg: &str, chars: u64) -> String {
    if chars > 0 {
        format!("{frame} {msg} {} chars", thousands(chars))
    } else {
        format!("{frame} {msg}")
    }
}

/// Decimal digits grouped in threes with commas: 1204 -> "1,204".
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{spinner_line, thousands};

    #[test]
    fn thousands_groups_in_threes() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(9), "9");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1204), "1,204");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn line_appends_count_only_once_content_arrives() {
        // A reasoning model sends no content at first: keep the bare
        // message rather than a misleading "0 chars".
        assert_eq!(spinner_line('⠋', "asking m...", 0), "⠋ asking m...");
        assert_eq!(
            spinner_line('⠙', "asking m...", 1204),
            "⠙ asking m... 1,204 chars"
        );
    }
}
