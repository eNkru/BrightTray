//! Client side of the BrightTray helper pipe.
//!
//! The hook runs inside ShellHost, so it must never keep a monitor handle of its own: the
//! helper is the only process that talks DDC/CI. All calls here are best-effort and fail
//! silently when the helper is not running.

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;

const PIPE_NAME: &str = r"\\.\pipe\BrightTray";
const WAIT_MS: u32 = 250;

fn wait_for_pipe() {
    unsafe {
        use windows::core::w;
        use windows::Win32::System::Pipes::WaitNamedPipeW;
        let _ = WaitNamedPipeW(w!(r"\\.\pipe\BrightTray"), WAIT_MS);
    }
}

fn connect() -> Option<File> {
    wait_for_pipe();
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE_NAME)
        .ok()
}

/// Current brightness (0-100) as reported by the helper, or `None` if it is unreachable.
/// Blocking: only call this from a worker thread.
pub fn get_all() -> Option<u32> {
    let mut file = connect()?;
    file.write_all(b"getall\n").ok()?;
    file.flush().ok()?;

    let mut line = String::new();
    BufReader::new(file).read_line(&mut line).ok()?;
    line.trim().parse::<u32>().ok().map(|p| p.min(100))
}

/// Asks the helper to apply `percent` to the monitors it controls. Never blocks: `SetValue`
/// is called on ShellHost's UI thread while the slider is being dragged, so the value is
/// handed to a writer thread that keeps one connection open and only sends the latest value.
pub fn set_all(percent: u32) {
    static SENDER: OnceLock<Sender<u32>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (tx, rx) = channel();
        std::thread::spawn(move || writer(rx));
        tx
    });
    let _ = sender.send(percent.min(100));
}

fn writer(rx: Receiver<u32>) {
    let mut connection: Option<File> = None;
    while let Ok(mut percent) = rx.recv() {
        // Collapse a burst of slider steps into the last one.
        for newer in rx.try_iter() {
            percent = newer;
        }
        // One reconnect per value: the helper may have restarted since the last write.
        for _ in 0..2 {
            if connection.is_none() {
                connection = connect();
            }
            let Some(file) = connection.as_mut() else {
                break;
            };
            if writeln!(file, "setall {percent}")
                .and_then(|()| file.flush())
                .is_ok()
            {
                break;
            }
            connection = None;
        }
    }
}
