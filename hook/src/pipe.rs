//! Client side of the BrightTray helper pipe.
//!
//! The hook runs inside ShellHost, so it must never keep a monitor handle of its own: the
//! helper is the only process that talks DDC/CI. All calls here are best-effort and fail
//! silently when the helper is not running.

use std::io::{BufRead, BufReader, Write};

const PIPE_NAME: &str = r"\\.\pipe\BrightTray";
const WAIT_MS: u32 = 250;

fn wait_for_pipe() {
    #[cfg(windows)]
    unsafe {
        use windows::core::w;
        use windows::Win32::System::Pipes::WaitNamedPipeW;
        let _ = WaitNamedPipeW(w!(r"\\.\pipe\BrightTray"), WAIT_MS);
    }
}

/// Current brightness (0-100) as reported by the helper, or `None` if it is unreachable.
pub fn get_all() -> Option<u32> {
    wait_for_pipe();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE_NAME)
        .ok()?;
    file.write_all(b"getall\n").ok()?;
    file.flush().ok()?;

    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    line.trim().parse::<u32>().ok().map(|p| p.min(100))
}

/// Asks the helper to apply `percent` to every DDC/CI-capable monitor. Fire-and-forget: the
/// helper coalesces rapid updates, which is exactly what dragging a slider produces.
pub fn set_all(percent: u32) {
    wait_for_pipe();
    let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(PIPE_NAME) else {
        return;
    };
    let _ = writeln!(file, "setall {}", percent.min(100));
    let _ = file.flush();
}
