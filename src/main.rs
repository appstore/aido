use std::io::Write as _;

/// `aido | head` should die silently like any other unix tool, not panic.
#[cfg(unix)]
fn restore_sigpipe() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_sigpipe() {}

#[tokio::main]
async fn main() {
    restore_sigpipe();
    let code = aido::app::run().await;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
