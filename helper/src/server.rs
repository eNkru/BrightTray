//! Named pipe server: `\\.\pipe\BrightTray`, line-based text protocol, one thread per client.
//!
//!   list            -> one line per monitor "index\tcurrent\tmax\tname", then an empty line
//!   get <index>     -> "current max" or "err"
//!   set <index> <p> -> no reply; p is 0-100. Rapid sets are coalesced and paced (see below).
//!   getall          -> "percent" of the first targeted DDC/CI-capable monitor, or "err"
//!   setall <p>      -> no reply; applies p to every targeted DDC/CI-capable monitor
//!
//! `getall` / `setall` are what the Quick Settings hook uses. Which monitors they reach is the
//! server's [`Target`]: every monitor for the CLI, the one the user picked for the GUI.
//! While serving, moves of the Windows brightness slider are mirrored to the target as well.
//!
//! Writes are paced rather than verified per step: a drag sends at most one write per monitor
//! per [`WRITE_INTERVAL`], always carrying the newest value, and the read-back happens once
//! [`VERIFY_DELAY`] after the last write. A read (`get`, `getall`, `list`) writes the pending
//! value out first, so its answer is the value that was last asked for.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

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

/// Minimum interval between two writes to the same monitor. Every write is an EEPROM write on
/// the monitor, so a slider drag must not turn every step into one.
const WRITE_INTERVAL: Duration = Duration::from_millis(300);
/// How long after the last write the read-back runs. A drag therefore costs one read, not one
/// per step, while the user still gets told when a monitor ignored the final value.
///
/// Must stay greater than [`WRITE_INTERVAL`]: a resend opens the throttle window without
/// consulting [`write_allowed`], which is only safe while the read-back cannot fall due inside
/// that window. Retuning this below [`WRITE_INTERVAL`] would silently break the per-monitor
/// write cadence.
const VERIFY_DELAY: Duration = Duration::from_millis(500);

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

/// Owns the monitor handles and the write cadence. DDC/CI is slow (~50 ms per command) and
/// every write is an EEPROM write, so sets that pile up are collapsed to the latest value per
/// monitor and written no more than once per [`WRITE_INTERVAL`].
fn worker(rx: Receiver<Cmd>, mut target: Target) {
    let mut monitors = ddc::enumerate();
    // Which monitors answered DDC/CI at the last probe, so slider moves skip the others instead
    // of paying the retry delay on every step. Probed lazily; reset whenever monitors change.
    let mut capable: Option<Vec<bool>> = None;
    // The value that still has to be written, per monitor: the newest one, never a backlog.
    let mut pending: BTreeMap<usize, u32> = BTreeMap::new();
    // When the last write to each monitor happened, to keep [`WRITE_INTERVAL`].
    let mut throttle: BTreeMap<usize, Instant> = BTreeMap::new();
    // The trailing read-back: the value that was written, and when it is due.
    let mut verify: BTreeMap<usize, (u32, Instant)> = BTreeMap::new();

    fn probe(monitors: &[ddc::Monitor]) -> Vec<bool> {
        monitors.iter().map(|m| m.brightness().is_some()).collect()
    }

    loop {
        // Sleep until the next write or read-back is due. With nothing scheduled there is no
        // deadline and the worker blocks until a command arrives.
        let first = match next_deadline(&pending, &throttle, &verify) {
            Some(deadline) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(wait) {
                    Ok(cmd) => Some(cmd),
                    // The deadline is here; the work happens below.
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            },
        };

        if let Some(first) = first {
            // A burst becomes one pass: 100 steps of a drag write once, not 100 times.
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
                        if flush(&mut monitors, &mut pending, &mut throttle, &mut verify) {
                            capable = None;
                        }
                        let line = match monitors.get(i).and_then(|m| m.brightness()) {
                            Some(b) => format!("{} {}\n", b.current, b.max),
                            None => "err\n".to_string(),
                        };
                        let _ = reply.send(line);
                    }
                    Cmd::GetAll(reply) => {
                        if flush(&mut monitors, &mut pending, &mut throttle, &mut verify) {
                            capable = None;
                        }
                        // The hook polls this; skip monitors already known not to answer so
                        // each poll does not pay their retry delay.
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
                        flush(&mut monitors, &mut pending, &mut throttle, &mut verify);
                        // Re-enumerate so hot-plugged monitors show up. The cadence state follows
                        // the monitors that survived, by key rather than by index.
                        reenumerate(&mut monitors, &mut pending, &mut throttle, &mut verify);
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
        }

        if write_due(&mut monitors, &mut pending, &mut throttle, &mut verify) {
            capable = None;
        }
        if verify_due(&mut monitors, &mut pending, &mut throttle, &mut verify) {
            capable = None;
        }
    }
}

/// The earliest moment the worker has to wake up: when a throttled value may be written, or
/// when a trailing read-back falls due.
fn next_deadline(
    pending: &BTreeMap<usize, u32>,
    throttle: &BTreeMap<usize, Instant>,
    verify: &BTreeMap<usize, (u32, Instant)>,
) -> Option<Instant> {
    let writes = pending.keys().map(|i| match throttle.get(i) {
        Some(last) => *last + WRITE_INTERVAL,
        // Never written to: the first value of a drag goes out immediately.
        None => Instant::now(),
    });
    let reads = verify.values().map(|(_, due)| *due);
    writes.chain(reads).min()
}

/// Whether the throttle window for a monitor has passed, so a write may go out now.
fn write_allowed(last: Option<&Instant>) -> bool {
    match last {
        Some(last) => last.elapsed() >= WRITE_INTERVAL,
        None => true,
    }
}

/// Writes the pending values whose throttle window has passed. The pending entry holds the
/// newest value, so a throttled write is late but never stale. Returns true if the monitor list
/// was re-enumerated.
fn write_due(
    monitors: &mut Vec<ddc::Monitor>,
    pending: &mut BTreeMap<usize, u32>,
    throttle: &mut BTreeMap<usize, Instant>,
    verify: &mut BTreeMap<usize, (u32, Instant)>,
) -> bool {
    let due: Vec<(usize, u32)> = pending
        .iter()
        .filter(|&(&i, _)| write_allowed(throttle.get(&i)))
        .map(|&(&i, &p)| (i, p))
        .collect();
    for &(i, _) in &due {
        pending.remove(&i);
    }
    write_values(monitors, due, pending, throttle, verify)
}

/// Writes every pending value out now, ignoring the throttle window: a read has to answer with
/// the value that was last requested, not with the one that happened to reach the monitor.
/// Returns true if the monitor list was re-enumerated.
fn flush(
    monitors: &mut Vec<ddc::Monitor>,
    pending: &mut BTreeMap<usize, u32>,
    throttle: &mut BTreeMap<usize, Instant>,
    verify: &mut BTreeMap<usize, (u32, Instant)>,
) -> bool {
    let values: Vec<(usize, u32)> = std::mem::take(pending).into_iter().collect();
    write_values(monitors, values, pending, throttle, verify)
}

/// Sends `values` in monitor order, records the write time and schedules the trailing read-back.
///
/// The batch is resolved to a [`ddc::Monitor::key`] before the first write, so a stale-handle
/// refresh in the middle of a multi-monitor batch cannot send the remaining values to the wrong
/// monitor. Returns true if the monitor list was re-enumerated.
fn write_values(
    monitors: &mut Vec<ddc::Monitor>,
    values: Vec<(usize, u32)>,
    pending: &mut BTreeMap<usize, u32>,
    throttle: &mut BTreeMap<usize, Instant>,
    verify: &mut BTreeMap<usize, (u32, Instant)>,
) -> bool {
    // A client can name any index: one that is not a monitor has nothing to write to and must
    // not leave a throttle entry behind for a monitor that never existed.
    let batch: Vec<(String, u32)> = values
        .into_iter()
        .filter_map(|(i, p)| monitors.get(i).map(|m| (m.key(), p)))
        .collect();
    let mut refreshed = false;
    for (key, p) in batch {
        let ok = match index_of(monitors, &key) {
            Some(i) => monitors[i].write_brightness_percent(p),
            None => false,
        };
        if !ok && !refreshed {
            // Handles go stale after sleep or hot-plug: refresh once and retry.
            reenumerate(monitors, pending, throttle, verify);
            refreshed = true;
            if let Some(i) = index_of(monitors, &key) {
                monitors[i].write_brightness_percent(p);
            }
        }
        let Some(i) = index_of(monitors, &key) else {
            // The monitor went away with the refresh; there is nothing to pace or verify.
            continue;
        };
        let now = Instant::now();
        throttle.insert(i, now);
        // The newest write replaces the scheduled read-back of an older one, so a drag is
        // verified once, after it has settled, rather than once per step.
        verify.insert(i, (p, now + VERIFY_DELAY));
    }
    refreshed
}

/// The position of the monitor whose [`ddc::Monitor::key`] is `key` in the current enumeration.
fn index_of(monitors: &[ddc::Monitor], key: &str) -> Option<usize> {
    monitors.iter().position(|m| m.key() == key)
}

/// Replaces the monitor list and carries the cadence state over to the monitors that are still
/// there. Indices are only meaningful for one enumeration, so the state is re-keyed by
/// [`ddc::Monitor::key`] instead of by position. A monitor that disappeared takes its pending
/// write, throttle and read-back with it — there is nothing left to write to or read from.
fn reenumerate(
    monitors: &mut Vec<ddc::Monitor>,
    pending: &mut BTreeMap<usize, u32>,
    throttle: &mut BTreeMap<usize, Instant>,
    verify: &mut BTreeMap<usize, (u32, Instant)>,
) {
    let old_keys: Vec<String> = monitors.iter().map(|m| m.key()).collect();
    *monitors = ddc::enumerate();
    let new_index: HashMap<String, usize> = monitors
        .iter()
        .enumerate()
        .map(|(i, m)| (m.key(), i))
        .collect();
    let remap = |old: usize| -> Option<usize> {
        old_keys
            .get(old)
            .and_then(|key| new_index.get(key).copied())
    };
    *pending = std::mem::take(pending)
        .into_iter()
        .filter_map(|(i, p)| remap(i).map(|i| (i, p)))
        .collect();
    *throttle = std::mem::take(throttle)
        .into_iter()
        .filter_map(|(i, at)| remap(i).map(|i| (i, at)))
        .collect();
    *verify = std::mem::take(verify)
        .into_iter()
        .filter_map(|(i, v)| remap(i).map(|i| (i, v)))
        .collect();
}

/// Runs the trailing read-backs that are due. A monitor that answered with another value is
/// sent the write once more, and that resend is not verified again — asking a second time will
/// not change its mind either. Returns true if the monitor list was re-enumerated.
fn verify_due(
    monitors: &mut Vec<ddc::Monitor>,
    pending: &mut BTreeMap<usize, u32>,
    throttle: &mut BTreeMap<usize, Instant>,
    verify: &mut BTreeMap<usize, (u32, Instant)>,
) -> bool {
    let now = Instant::now();
    // The monitors to read back, identified by key: an index only means something for one
    // enumeration, and a re-enumeration below may reorder the list.
    let due: Vec<(usize, String, u32)> = verify
        .iter()
        .filter(|&(_, &(_, at))| at <= now)
        .filter_map(|(&i, &(p, _))| monitors.get(i).map(|m| (i, m.key(), p)))
        .collect();
    // Drop every due entry up front, by its current index, before any re-enumeration can
    // re-key the map; the loop below then works from the stable keys in `due`.
    for &(i, _, _) in &due {
        verify.remove(&i);
    }
    let mut refreshed = false;
    for (_, key, p) in due {
        let matched = match index_of(monitors, &key) {
            Some(i) => monitors[i].verify_brightness(p),
            // The monitor is gone: nothing to read back and nowhere to resend.
            None => continue,
        };
        if matched {
            continue;
        }
        let sent = match index_of(monitors, &key) {
            Some(i) => monitors[i].write_brightness_percent(p),
            None => false,
        };
        if !sent && !refreshed {
            reenumerate(monitors, pending, throttle, verify);
            refreshed = true;
            if let Some(i) = index_of(monitors, &key) {
                monitors[i].write_brightness_percent(p);
            }
        }
        // A resend is a write: it opens the same window, so the next value of a drag waits.
        if let Some(i) = index_of(monitors, &key) {
            throttle.insert(i, Instant::now());
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
