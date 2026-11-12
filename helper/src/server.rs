//! Named pipe server: `\\.\pipe\BrightTray`, line-based text protocol, one thread per client.
//!
//!   list            -> one line per monitor "index\tcurrent\tmax\tdescription", then an empty line
//!   get <index>     -> "current max" or "err"
//!   set <index> <p> -> no reply; p is 0-100. Rapid sets are coalesced (last value wins).
//!   getall          -> "percent" of the first DDC/CI-capable monitor, or "err"
//!   setall <p>      -> no reply; applies p to every DDC/CI-capable monitor
//!
//! While serving, moves of the Windows brightness slider are mirrored to every DDC/CI monitor.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::mpsc::{channel, Receiver, Sender};

use windows::core::w;
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use crate::{ddc, slider};

enum Cmd {
    Set(usize, u32),
    /// From the Windows brightness slider or the injected hook: all DDC/CI monitors.
    SetAll(u32),
    Get(usize, Sender<String>),
    GetAll(Sender<String>),
    List(Sender<String>),
}

/// Owns the monitor handles. DDC/CI is slow (~50 ms per command), so sets that pile up
/// while one is in flight are collapsed to the latest value per monitor.
fn worker(rx: Receiver<Cmd>) {
    let mut monitors = ddc::enumerate();
    // Which monitors answered DDC/CI at the last probe, so slider moves skip the others instead
    // of paying the retry delay on every step. Probed lazily; reset whenever monitors change.
    let mut capable: Option<Vec<bool>> = None;
    let mut pending: BTreeMap<usize, u32> = BTreeMap::new();

    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter());

        for cmd in batch {
            match cmd {
                Cmd::Set(i, p) => {
                    pending.insert(i, p);
                }
                Cmd::SetAll(p) => {
                    let capable = capable.get_or_insert_with(|| {
                        monitors.iter().map(|m| m.brightness().is_some()).collect()
                    });
                    for (i, _) in capable.iter().enumerate().filter(|(_, ok)| **ok) {
                        pending.insert(i, p);
                    }
                }
                Cmd::Get(i, reply) => {
                    flush(&mut monitors, &mut pending);
                    let line = match monitors.get(i).and_then(|m| m.brightness()) {
                        Some(b) => format!("{} {}\n", b.current, b.max),
                        None => "err\n".to_string(),
                    };
                    let _ = reply.send(line);
                }
                Cmd::GetAll(reply) => {
                    flush(&mut monitors, &mut pending);
                    let line = monitors
                        .iter()
                        .find_map(|m| m.brightness())
                        .map(|b| {
                            let percent = (b.current * 100 + b.max / 2) / b.max;
                            format!("{}\n", percent.min(100))
                        })
                        .unwrap_or_else(|| "err\n".to_string());
                    let _ = reply.send(line);
                }
                Cmd::List(reply) => {
                    flush(&mut monitors, &mut pending);
                    // Re-enumerate so hot-plugged monitors show up.
                    monitors = ddc::enumerate();
                    capable = None;
                    let mut out = String::new();
                    for (i, m) in monitors.iter().enumerate() {
                        let (cur, max) = m
                            .brightness()
                            .map(|b| (b.current as i64, b.max as i64))
                            .unwrap_or((-1, -1));
                        out.push_str(&format!("{i}\t{cur}\t{max}\t{}\n", m.description));
                    }
                    out.push('\n');
                    let _ = reply.send(out);
                }
            }
        }
        if flush(&mut monitors, &mut pending) {
            capable = None;
        }
    }
}

/// Applies pending sets. Returns true if the monitor list was re-enumerated.
fn flush(monitors: &mut Vec<ddc::Monitor>, pending: &mut BTreeMap<usize, u32>) -> bool {
    let mut refreshed = false;
    for (i, p) in std::mem::take(pending) {
        let ok = monitors.get(i).is_some_and(|m| m.set_brightness_percent(p));
        if !ok && !refreshed {
            // Handles go stale after sleep or hot-plug: refresh once and retry.
            *monitors = ddc::enumerate();
            refreshed = true;
            if let Some(m) = monitors.get(i) {
                m.set_brightness_percent(p);
            }
        }
    }
    refreshed
}

fn handle_client(pipe: &File, tx: &Sender<Cmd>) -> std::io::Result<()> {
    let mut out = pipe;
    for line in BufReader::new(pipe).lines() {
        let line = line?;
        let mut parts = line.split_whitespace();
        let (reply_tx, reply_rx) = channel();
        let sent = match (parts.next(), parts.next(), parts.next()) {
            (Some("list"), None, None) => tx.send(Cmd::List(reply_tx)).is_ok(),
            (Some("get"), Some(i), None) => match i.parse() {
                Ok(i) => tx.send(Cmd::Get(i, reply_tx)).is_ok(),
                Err(_) => false,
            },
            (Some("getall"), None, None) => tx.send(Cmd::GetAll(reply_tx)).is_ok(),
            (Some("set"), Some(i), Some(p)) => {
                if let (Ok(i), Ok(p)) = (i.parse(), p.parse()) {
                    let _ = tx.send(Cmd::Set(i, p));
                }
                continue;
            }
            (Some("setall"), Some(p), None) => {
                if let Ok(p) = p.parse() {
                    let _ = tx.send(Cmd::SetAll(p));
                }
                continue;
            }
            _ => false,
        };
        let reply = if sent {
            reply_rx.recv().unwrap_or_else(|_| "err\n".to_string())
        } else {
            "err\n".to_string()
        };
        out.write_all(reply.as_bytes())?;
        out.flush()?;
    }
    Ok(())
}

pub fn serve() -> std::io::Result<()> {
    let (tx, rx) = channel();
    std::thread::spawn(move || worker(rx));

    // The first notification is only the slider's current position, delivered on registration.
    // Mirror actual moves only, so starting the helper never changes a monitor by itself.
    let slider_state = std::sync::Mutex::new((tx.clone(), None::<u32>));
    let _slider = slider::subscribe(move |p| {
        let mut state = slider_state.lock().unwrap();
        let (tx, last) = &mut *state;
        if last.is_some_and(|l| l != p) {
            let _ = tx.send(Cmd::SetAll(p));
        }
        *last = Some(p);
    })
    .map_err(|e| eprintln!("not following the Windows brightness slider: {e}"))
    .ok();

    loop {
        let handle = unsafe {
            CreateNamedPipeW(
                w!(r"\\.\pipe\BrightTray"),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                None,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // The File owns the handle from here on and closes it on drop.
        let pipe = unsafe { File::from_raw_handle(handle.0 as _) };

        if let Err(e) = unsafe { ConnectNamedPipe(handle, None) } {
            if e.code() != ERROR_PIPE_CONNECTED.to_hresult() {
                continue;
            }
        }
        // Serve each client on its own thread so the tray app and the injected hook can be
        // connected at the same time.
        let client_tx = tx.clone();
        std::thread::spawn(move || {
            // A client disconnecting mid-line is normal; just wait for the next one.
            let _ = handle_client(&pipe, &client_tx);
        });
    }
}
