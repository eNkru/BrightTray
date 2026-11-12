//! Named pipe server: `\\.\pipe\BrightTray`, line-based text protocol, one thread per client.
//!
//!   list            -> one line per monitor "index\tcurrent\tmax\tdescription", then an empty line
//!   get <index>     -> "current max" or "err"
//!   set <index> <p> -> no reply; p is 0-100. Rapid sets are coalesced (last value wins).
//!   getall          -> "percent" of the first targeted DDC/CI-capable monitor, or "err"
//!   setall <p>      -> no reply; applies p to every targeted DDC/CI-capable monitor
//!
//! `getall` / `setall` are what the Quick Settings hook uses. Which monitors they reach is the
//! server's [`Target`]: every monitor for the CLI, the one the user picked for the GUI.
//! While serving, moves of the Windows brightness slider are mirrored to the target as well.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::mpsc::{channel, Receiver, Sender};

use windows::core::w;
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use crate::{ddc, slider};

/// Which monitors `getall` / `setall` and the Windows slider act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Every DDC/CI-capable monitor.
    All,
    /// One monitor, found by [`ddc::Monitor::key`]; if the layout changed and the key is gone,
    /// by its name as long as exactly one monitor has it.
    Monitor { key: String, name: String },
}

impl Target {
    fn indices(&self, monitors: &[ddc::Monitor]) -> Vec<usize> {
        match self {
            Target::All => (0..monitors.len()).collect(),
            Target::Monitor { key, name } => {
                let by_key: Vec<usize> = monitors
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| &m.key() == key)
                    .map(|(i, _)| i)
                    .collect();
                if !by_key.is_empty() {
                    return by_key;
                }
                let by_name: Vec<usize> = monitors
                    .iter()
                    .enumerate()
                    .filter(|(_, m)| &m.name == name)
                    .map(|(i, _)| i)
                    .collect();
                if by_name.len() == 1 {
                    by_name
                } else {
                    Vec::new()
                }
            }
        }
    }
}

enum Cmd {
    Set(usize, u32),
    /// From the Windows brightness slider or the injected hook: the targeted monitors.
    SetAll(u32),
    Get(usize, Sender<String>),
    GetAll(Sender<String>),
    List(Sender<String>),
    SetTarget(Target),
}

/// Owns the monitor handles. DDC/CI is slow (~50 ms per command), so sets that pile up
/// while one is in flight are collapsed to the latest value per monitor.
fn worker(rx: Receiver<Cmd>, mut target: Target) {
    let mut monitors = ddc::enumerate();
    // Which monitors answered DDC/CI at the last probe, so slider moves skip the others instead
    // of paying the retry delay on every step. Probed lazily; reset whenever monitors change.
    let mut capable: Option<Vec<bool>> = None;
    let mut pending: BTreeMap<usize, u32> = BTreeMap::new();

    fn probe(monitors: &[ddc::Monitor]) -> Vec<bool> {
        monitors.iter().map(|m| m.brightness().is_some()).collect()
    }

    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        batch.extend(rx.try_iter());

        for cmd in batch {
            match cmd {
                Cmd::Set(i, p) => {
                    pending.insert(i, p);
                }
                Cmd::SetAll(p) => {
                    let capable = capable.get_or_insert_with(|| probe(&monitors));
                    for i in target.indices(&monitors) {
                        if capable.get(i).copied().unwrap_or(false) {
                            pending.insert(i, p);
                        }
                    }
                }
                Cmd::SetTarget(new_target) => {
                    target = new_target;
                }
                Cmd::Get(i, reply) => {
                    if flush(&mut monitors, &mut pending) {
                        capable = None;
                    }
                    let line = match monitors.get(i).and_then(|m| m.brightness()) {
                        Some(b) => format!("{} {}\n", b.current, b.max),
                        None => "err\n".to_string(),
                    };
                    let _ = reply.send(line);
                }
                Cmd::GetAll(reply) => {
                    if flush(&mut monitors, &mut pending) {
                        capable = None;
                    }
                    // The hook polls this; skip monitors already known not to answer so each
                    // poll does not pay their retry delay.
                    let capable = capable.get_or_insert_with(|| probe(&monitors));
                    let line = target
                        .indices(&monitors)
                        .into_iter()
                        .filter(|&i| capable.get(i).copied().unwrap_or(false))
                        .find_map(|i| monitors[i].brightness())
                        .map(|b| format!("{}\n", b.percent()))
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
                        out.push_str(&format!("{i}\t{cur}\t{max}\t{}\n", m.name));
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

fn create_instance(first: bool) -> std::io::Result<HANDLE> {
    // The first instance claims the name: if another helper already serves it, fail loudly
    // instead of silently running two servers and two injectors side by side.
    let open_mode = if first {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX
    };
    let handle = unsafe {
        CreateNamedPipeW(
            w!(r"\\.\pipe\BrightTray"),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            None,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = std::io::Error::last_os_error();
        if first && error.raw_os_error() == Some(ERROR_ACCESS_DENIED.0 as i32) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                r"\\.\pipe\BrightTray is already served by another BrightTray instance",
            ));
        }
        return Err(error);
    }
    Ok(handle)
}

fn accept_loop(mut handle: HANDLE, tx: Sender<Cmd>) {
    loop {
        // The File owns the handle from here on and closes it on drop.
        let pipe = unsafe { File::from_raw_handle(handle.0 as _) };
        let connected = match unsafe { ConnectNamedPipe(handle, None) } {
            Ok(()) => true,
            Err(e) => e.code() == ERROR_PIPE_CONNECTED.to_hresult(),
        };
        if connected {
            // Serve each client on its own thread so the GUI, scripts and the injected hook
            // can be connected at the same time.
            let client_tx = tx.clone();
            std::thread::spawn(move || {
                // A client disconnecting mid-line is normal; just wait for the next one.
                let _ = handle_client(&pipe, &client_tx);
            });
        }
        handle = loop {
            match create_instance(false) {
                Ok(next) => break next,
                Err(e) => {
                    eprintln!("pipe server: {e}");
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        };
    }
}

/// A running pipe server. It keeps serving until the process exits; dropping this only stops
/// mirroring the Windows brightness slider.
pub struct Server {
    tx: Sender<Cmd>,
    _slider: Option<slider::Subscription>,
}

impl Server {
    /// Claims the pipe name (failing if another instance owns it) and starts serving.
    pub fn start(target: Target) -> std::io::Result<Server> {
        let first = create_instance(true)?;

        let (tx, rx) = channel();
        std::thread::spawn(move || worker(rx, target));

        // The first notification is only the slider's current position, delivered on
        // registration. Mirror actual moves only, so starting never changes a monitor by itself.
        let slider_state = std::sync::Mutex::new((tx.clone(), None::<u32>));
        let slider = slider::subscribe(move |p| {
            let mut state = slider_state.lock().unwrap_or_else(|e| e.into_inner());
            let (tx, last) = &mut *state;
            if last.is_some_and(|l| l != p) {
                let _ = tx.send(Cmd::SetAll(p));
            }
            *last = Some(p);
        })
        .map_err(|e| eprintln!("not following the Windows brightness slider: {e}"))
        .ok();

        // HANDLE is a raw pointer; move it across as an integer.
        let raw = first.0 as isize;
        let accept_tx = tx.clone();
        std::thread::spawn(move || accept_loop(HANDLE(raw as _), accept_tx));

        Ok(Server {
            tx,
            _slider: slider,
        })
    }

    /// Switches which monitors `getall` / `setall` act on. Takes effect for the next command.
    pub fn set_target(&self, target: Target) {
        let _ = self.tx.send(Cmd::SetTarget(target));
    }
}

/// CLI entry point: serve every DDC/CI monitor until the process is killed.
pub fn serve() -> std::io::Result<()> {
    let _server = Server::start(Target::All)?;
    loop {
        std::thread::park();
    }
}
