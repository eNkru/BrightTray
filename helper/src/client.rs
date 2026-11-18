//! Client side of `\\.\pipe\BrightTray`, for callers inside this process tree.
//!
//! The helper's worker thread is the only thing in the process group that opens a monitor
//! handle: it owns the pacing state, the retry after a stale handle and the read-back. The
//! app therefore never calls `ddc::` for a write — it asks the worker through the pipe, the
//! same way the shell hook does. See
//! `spec/brighttray-helper/backend/quality-guidelines.md`.
//!
//! Unlike `hook/src/pipe.rs` these calls **block**: they run on the app's own threads (the
//! brightness-key watcher), never on a shell UI thread, so waiting for an answer is fine.
//! The hook keeps its own non-blocking copy on purpose — do not unify the two.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};

const PIPE_NAME: &str = r"\\.\pipe\BrightTray";
/// How long to wait for a free pipe instance before giving up.
const WAIT_MS: u32 = 250;

/// Opens the pipe for one request. `None` when no server is listening.
fn connect() -> Option<File> {
    unsafe {
        use windows::core::w;
        use windows::Win32::System::Pipes::WaitNamedPipeW;
        // A server that is busy creating its next instance is normal: wait briefly.
        let _ = WaitNamedPipeW(w!(r"\\.\pipe\BrightTray"), WAIT_MS);
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE_NAME)
        .ok()
}

/// The brightness the helper last measured, 0-100, or `None` when it cannot answer.
///
/// This is the monitor's own reading, not a value the caller remembers, so a brightness that
/// was changed somewhere else (the Quick Settings slider, the monitor's own OSD) is respected.
pub fn get_all() -> Option<u32> {
    let mut file = connect()?;
    file.write_all(b"getall\n").ok()?;
    file.flush().ok()?;

    let mut reply = String::new();
    BufReader::new(file).read_line(&mut reply).ok()?;
    // The reply is "<percent> <max>"; anything else (an "err") is not a brightness.
    reply
        .split_whitespace()
        .next()?
        .parse::<u32>()
        .ok()
        .map(|percent| percent.min(100))
}

/// Asks the worker to apply `percent` to whatever its current target selects.
///
/// A write never gets an answer by design (the shell must not wait on it), so a successful
/// send is all the confirmation there is. Returns false only when the pipe is unreachable.
pub fn set_all(percent: u32) -> bool {
    let Some(mut file) = connect() else {
        return false;
    };
    writeln!(file, "setall {}", percent.min(100))
        .and_then(|()| file.flush())
        .is_ok()
}
