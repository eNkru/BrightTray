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
//! value out first, so its answer is the value that was last asked for — unless the rate cap
//! defers that write, in which case the read answers with the value the monitor shows now.
//!
//! The pipe is created with an explicit security descriptor: full access for the current user's
//! SID only, plus a medium-integrity mandatory label, so no other local account can connect and
//! no low-integrity process can write. The write rate is capped per monitor at
//! [`WRITE_RATE_LIMIT`] writes per second; a write that would exceed the cap is deferred, never
//! dropped.

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::{w, HSTRING, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, BOOL, ERROR_ACCESS_DENIED, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL,
    INVALID_HANDLE_VALUE,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

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
/// How many monitor writes per second are allowed, per monitor. No monitor may be written more
/// than this often, whichever code path asks for the write — the normal write path, the write a
/// read forces out, the trailing read-back resend, or the stale-handle retry. A write that would
/// exceed the cap is deferred, never dropped.
const WRITE_RATE_LIMIT: u32 = 10;

/// The shortest gap the budget allows between two writes to the same monitor: one second over
/// [`WRITE_RATE_LIMIT`].
const BUDGET_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / WRITE_RATE_LIMIT as u64);

/// A per-monitor write budget: at most one write per [`BUDGET_INTERVAL`].
///
/// The budget is a token bucket that holds a single token and refills [`WRITE_RATE_LIMIT`] times
/// per second. A write takes the token; when the token is gone the write is deferred, not
/// dropped. The capacity of one is what makes the cap strict: no one-second window ever holds
/// more than [`WRITE_RATE_LIMIT`] writes, not even in a burst.
struct WriteBudget {
    /// Whether the token is available.
    ready: bool,
    /// When the token becomes available again.
    next: Instant,
}

impl WriteBudget {
    fn new(now: Instant) -> Self {
        WriteBudget {
            ready: true,
            next: now,
        }
    }

    /// Consumes the token when it is available. `false` means the budget is empty and the write
    /// must be deferred.
    fn take(&mut self, now: Instant) -> bool {
        if !self.ready && now >= self.next {
            self.ready = true;
        }
        if !self.ready {
            return false;
        }
        self.ready = false;
        self.next = now + BUDGET_INTERVAL;
        true
    }

    /// The earliest moment a write may go out again, for the worker's sleep deadline.
    fn next_token(&self, now: Instant) -> Instant {
        if self.ready || now >= self.next {
            now
        } else {
            self.next
        }
    }
}

/// Which monitors `getall` / `setall` and the Windows slider act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Every DDC/CI-capable monitor.
    All,
    /// One monitor, found by [`ddc::Monitor::key`]; if the layout changed and the key is gone,
    /// by its name as long as exactly one monitor has it.
    Monitor { key: String, name: String },
}

/// The monitors a target acts on, given their `(key, name)` pairs.
///
/// The key is the identity of a monitor and survives a re-enumeration as long as the display
/// layout does not change, so it is tried first. The name is only a fallback for a layout that
/// changed — and only when exactly one monitor carries it, because an ambiguous name must
/// resolve to nothing rather than to the wrong display.
fn select_indices(target: &Target, monitors: &[(String, String)]) -> Vec<usize> {
    match target {
        Target::All => (0..monitors.len()).collect(),
        Target::Monitor { key, name } => {
            let by_key: Vec<usize> = monitors
                .iter()
                .enumerate()
                .filter(|(_, (k, _))| k == key)
                .map(|(i, _)| i)
                .collect();
            if !by_key.is_empty() {
                return by_key;
            }
            let by_name: Vec<usize> = monitors
                .iter()
                .enumerate()
                .filter(|(_, (_, n))| n == name)
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

impl Target {
    fn indices(&self, monitors: &[ddc::Monitor]) -> Vec<usize> {
        let pairs: Vec<(String, String)> =
            monitors.iter().map(|m| (m.key(), m.name.clone())).collect();
        select_indices(self, &pairs)
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
    // The per-monitor write budget, to keep [`WRITE_RATE_LIMIT`] writes per second.
    let mut budgets: BTreeMap<usize, WriteBudget> = BTreeMap::new();

    fn probe(monitors: &[ddc::Monitor]) -> Vec<bool> {
        monitors.iter().map(|m| m.brightness().is_some()).collect()
    }

    loop {
        // Sleep until the next write or read-back is due. With nothing scheduled there is no
        // deadline and the worker blocks until a command arrives.
        let first = match next_deadline(&pending, &throttle, &verify, &budgets) {
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
                        if flush(
                            &mut monitors,
                            &mut pending,
                            &mut throttle,
                            &mut verify,
                            &mut budgets,
                        ) {
                            capable = None;
                        }
                        let line = match monitors.get(i).and_then(|m| m.brightness()) {
                            Some(b) => format!("{} {}\n", b.current, b.max),
                            None => "err\n".to_string(),
                        };
                        let _ = reply.send(line);
                    }
                    Cmd::GetAll(reply) => {
                        if flush(
                            &mut monitors,
                            &mut pending,
                            &mut throttle,
                            &mut verify,
                            &mut budgets,
                        ) {
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
                        flush(
                            &mut monitors,
                            &mut pending,
                            &mut throttle,
                            &mut verify,
                            &mut budgets,
                        );
                        // Re-enumerate so hot-plugged monitors show up. The cadence state follows
                        // the monitors that survived, by key rather than by index.
                        reenumerate(
                            &mut monitors,
                            &mut pending,
                            &mut throttle,
                            &mut verify,
                            &mut budgets,
                        );
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

        if write_due(
            &mut monitors,
            &mut pending,
            &mut throttle,
            &mut verify,
            &mut budgets,
        ) {
            capable = None;
        }
        if verify_due(
            &mut monitors,
            &mut pending,
            &mut throttle,
            &mut verify,
            &mut budgets,
        ) {
            capable = None;
        }
    }
}

/// The earliest moment the worker has to wake up: when a throttled value may be written, when
/// a trailing read-back falls due, or when a deferred write's budget has refilled a token.
fn next_deadline(
    pending: &BTreeMap<usize, u32>,
    throttle: &BTreeMap<usize, Instant>,
    verify: &BTreeMap<usize, (u32, Instant)>,
    budgets: &BTreeMap<usize, WriteBudget>,
) -> Option<Instant> {
    let now = Instant::now();
    let writes = pending.keys().map(|i| {
        let throttled = match throttle.get(i) {
            Some(last) => *last + WRITE_INTERVAL,
            // Never written to: the first value of a drag goes out immediately.
            None => now,
        };
        let budgeted = match budgets.get(i) {
            Some(budget) => budget.next_token(now),
            None => now,
        };
        throttled.max(budgeted)
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
    budgets: &mut BTreeMap<usize, WriteBudget>,
) -> bool {
    let due: Vec<(usize, u32)> = pending
        .iter()
        .filter(|&(&i, _)| write_allowed(throttle.get(&i)))
        .map(|(i, &p)| (*i, p))
        .collect();
    for &(i, _) in &due {
        pending.remove(&i);
    }
    write_values(monitors, due, pending, throttle, verify, budgets)
}

/// Writes every pending value out now, ignoring the throttle window: a read has to answer with
/// the value that was last requested, not with the one that happened to reach the monitor.
/// The rate cap still applies: a value the cap defers stays pending, and the read answers with
/// the value the monitor shows now. Returns true if the monitor list was re-enumerated.
fn flush(
    monitors: &mut Vec<ddc::Monitor>,
    pending: &mut BTreeMap<usize, u32>,
    throttle: &mut BTreeMap<usize, Instant>,
    verify: &mut BTreeMap<usize, (u32, Instant)>,
    budgets: &mut BTreeMap<usize, WriteBudget>,
) -> bool {
    let values: Vec<(usize, u32)> = std::mem::take(pending).into_iter().collect();
    write_values(monitors, values, pending, throttle, verify, budgets)
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
    budgets: &mut BTreeMap<usize, WriteBudget>,
) -> bool {
    // A client can name any index: one that is not a monitor has nothing to write to and must
    // not leave a throttle entry behind for a monitor that never existed.
    let batch: Vec<(String, u32)> = values
        .into_iter()
        .filter_map(|(i, p)| monitors.get(i).map(|m| (m.key(), p)))
        .collect();
    let mut refreshed = false;
    for (key, p) in batch {
        // The token is taken before the write, not after: a write the budget refuses must not
        // reach the monitor at all.
        let Some(i) = index_of(monitors, &key) else {
            continue;
        };
        let now = Instant::now();
        if !budgets
            .entry(i)
            .or_insert_with(|| WriteBudget::new(now))
            .take(now)
        {
            // The budget is exhausted: keep the value pending and write it on a later tick.
            pending.insert(i, p);
            continue;
        }
        let ok = monitors[i].write_brightness_percent(p);
        if !ok && !refreshed {
            // Handles go stale after sleep or hot-plug: refresh once and retry.
            reenumerate(monitors, pending, throttle, verify, budgets);
            refreshed = true;
            // The retry is a second write to the monitor, so it takes a token of its own. When
            // the budget is empty the value stays pending instead of going out unchecked.
            let Some(i) = index_of(monitors, &key) else {
                continue;
            };
            let now = Instant::now();
            if !budgets
                .entry(i)
                .or_insert_with(|| WriteBudget::new(now))
                .take(now)
            {
                pending.insert(i, p);
                continue;
            }
            monitors[i].write_brightness_percent(p);
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
    budgets: &mut BTreeMap<usize, WriteBudget>,
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
    *budgets = std::mem::take(budgets)
        .into_iter()
        .filter_map(|(i, b)| remap(i).map(|i| (i, b)))
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
    budgets: &mut BTreeMap<usize, WriteBudget>,
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
        // The resend is a write too: take a token first, so a lying monitor cannot double the
        // rate through the read-back path. When the budget is empty the resend is deferred.
        let Some(i) = index_of(monitors, &key) else {
            continue;
        };
        let now = Instant::now();
        let budget = budgets.entry(i).or_insert_with(|| WriteBudget::new(now));
        if !budget.take(now) {
            // Deferred, not dropped: the resend stays due and is tried once the budget refills.
            verify.insert(i, (p, budget.next_token(now)));
            continue;
        }
        let sent = monitors[i].write_brightness_percent(p);
        if !sent && !refreshed {
            // Handles go stale after sleep or hot-plug: refresh once and retry. The retry is a
            // write of its own, so it takes a token of its own.
            reenumerate(monitors, pending, throttle, verify, budgets);
            refreshed = true;
            let Some(i) = index_of(monitors, &key) else {
                continue;
            };
            let now = Instant::now();
            let budget = budgets.entry(i).or_insert_with(|| WriteBudget::new(now));
            if !budget.take(now) {
                // Deferred, not dropped: the resend stays due until the budget refills.
                verify.insert(i, (p, budget.next_token(now)));
                continue;
            }
            monitors[i].write_brightness_percent(p);
        }
        // A resend is a write: it opens the same window, so the next value of a drag waits.
        if let Some(i) = index_of(monitors, &key) {
            throttle.insert(i, Instant::now());
        }
    }
    refreshed
}

/// What one line of the pipe protocol asks for.
///
/// The variant says what the client wants; whether the client gets an answer is decided by
/// [`Command::replies`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    /// `list`: one line per monitor, then an empty line.
    List,
    /// `get <index>`: the brightness of that monitor.
    Get(usize),
    /// `getall`: the percent of the first targeted DDC/CI-capable monitor.
    GetAll,
    /// `set <index> <percent>`: a write to that monitor.
    Set(usize, u32),
    /// `setall <percent>`: a write to every targeted DDC/CI-capable monitor.
    SetAll(u32),
    /// The line does not match the protocol: answer `err`.
    Error,
    /// The line is a write whose value cannot be used: drop it and say nothing.
    Dropped,
}

impl Command {
    /// Whether the client gets a reply for this command.
    ///
    /// Writes never do, not even an `err`: the hook's writer thread sends a value on every
    /// slider step, and an acknowledgement would turn the drag into a request/response round
    /// trip on ShellHost's UI thread.
    fn replies(&self) -> bool {
        match self {
            Command::List | Command::Get(_) | Command::GetAll | Command::Error => true,
            Command::Set(_, _) | Command::SetAll(_) | Command::Dropped => false,
        }
    }
}

/// Parses one line of the pipe protocol into the command it asks for.
///
/// The line is split into at most three words, so a command has to match its exact arity;
/// anything else is [`Command::Error`]. A write whose value does not parse is
/// [`Command::Dropped`]: the command is thrown away and the client is told nothing.
fn parse_command(line: &str) -> Command {
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some("list"), None, None) => Command::List,
        (Some("get"), Some(i), None) => match i.parse() {
            Ok(i) => Command::Get(i),
            Err(_) => Command::Error,
        },
        (Some("getall"), None, None) => Command::GetAll,
        (Some("set"), Some(i), Some(p)) => match (i.parse(), p.parse()) {
            (Ok(i), Ok(p)) => Command::Set(i, p),
            _ => Command::Dropped,
        },
        (Some("setall"), Some(p), None) => match p.parse() {
            Ok(p) => Command::SetAll(p),
            Err(_) => Command::Dropped,
        },
        _ => Command::Error,
    }
}

fn handle_client(pipe: &File, tx: &Sender<Cmd>) -> std::io::Result<()> {
    let mut out = pipe;
    for line in BufReader::new(pipe).lines() {
        let line = line?;
        let command = parse_command(&line);
        // Writes answer nothing at all, so the answer is skipped before it is written.
        let replies = command.replies();
        let (reply_tx, reply_rx) = channel();
        let sent = match command {
            Command::List => tx.send(Cmd::List(reply_tx)).is_ok(),
            Command::Get(i) => tx.send(Cmd::Get(i, reply_tx)).is_ok(),
            Command::GetAll => tx.send(Cmd::GetAll(reply_tx)).is_ok(),
            Command::Set(i, p) => {
                let _ = tx.send(Cmd::Set(i, p));
                false
            }
            Command::SetAll(p) => {
                let _ = tx.send(Cmd::SetAll(p));
                false
            }
            // The line does not match the protocol.
            Command::Error => false,
            // The write is dropped silently: the client is the shell and must not wait.
            Command::Dropped => false,
        };
        if !replies {
            continue;
        }
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

/// Prints the fallback warning once: the pipe's security descriptor could not be built, so the
/// server keeps the default descriptor. The server must still start — a weaker ACL beats not
/// starting at all, but the user should know what is lost.
fn warn_default_security() {
    static WARNED: OnceLock<()> = OnceLock::new();
    if WARNED.set(()).is_ok() {
        eprintln!(
            "pipe server: could not build the pipe security descriptor; \
             any process running as the same user can connect"
        );
    }
}

/// UTF-16 → `String`, the same tiny helper the other modules of this crate use.
fn wide_to_string(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// The current user's SID as a string ("S-1-5-21-…"), read from the running process's own token
/// so the descriptor is correct for any account and locale. `None` when any step fails.
fn current_user_sid() -> Option<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        // The first call always fails (the buffer is too small) but fills in the length.
        let mut len: u32 = 0;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        if len == 0 {
            let _ = CloseHandle(token);
            return None;
        }
        let mut buffer = vec![0u8; len as usize];
        let mut returned: u32 = 0;
        let result = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr() as *mut c_void),
            len,
            &mut returned,
        );
        let _ = CloseHandle(token);
        result.ok()?;

        // `buffer` is a `Vec<u8>` (align 1), so read the struct out unaligned rather than
        // creating a reference to a possibly misaligned `TOKEN_USER`.
        let token_user = std::ptr::read_unaligned(buffer.as_ptr() as *const TOKEN_USER);
        let mut sid_string = PWSTR::null();
        ConvertSidToStringSidW(token_user.User.Sid, &mut sid_string).ok()?;
        let text = wide_to_string(sid_string.as_wide());
        let _ = LocalFree(HLOCAL(sid_string.0 as *mut c_void));
        Some(text)
    }
}

/// The pipe's security attributes.
///
/// `SECURITY_ATTRIBUTES` holds a raw pointer, so it is neither `Send` nor `Sync` on its own.
/// The descriptor behind it is built once, is never written to again and is only read — by the
/// kernel, when a pipe instance is created — so handing it to the threads that serve clients
/// cannot race.
struct PipeSecurity(SECURITY_ATTRIBUTES);

// SAFETY: the wrapped descriptor is immutable after it is built, so sharing it between
// threads is safe.
unsafe impl Send for PipeSecurity {}
// SAFETY: see above.
unsafe impl Sync for PipeSecurity {}

/// Builds the pipe's security descriptor from the current user's SID. `None` when any step
/// fails, so the caller falls back to the default descriptor instead of failing to start.
fn build_security_attributes() -> Option<PipeSecurity> {
    let sid = current_user_sid()?;
    // A protected DACL naming only the user, plus an explicit medium-integrity label. There is
    // deliberately no ACE for SYSTEM or the administrators group: the pipe belongs to the user
    // who started BrightTray, and to nobody else.
    let sddl_text = format!("D:P(A;;GA;;;{sid})S:(ML;;NW;;;ME)");
    let sddl = HSTRING::from(sddl_text.as_str());
    let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
    let mut size: u32 = 0;
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &sddl,
            SDDL_REVISION_1,
            &mut descriptor,
            Some(&mut size),
        )
        .ok()?;
    }
    Some(PipeSecurity(SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: BOOL(0),
    }))
}

/// The pipe's security descriptor, built once and kept for the lifetime of the process: every
/// pipe instance must present the same one. `None` when the descriptor could not be built, in
/// which case the server falls back to the default descriptor of its token.
fn security_attributes() -> Option<&'static SECURITY_ATTRIBUTES> {
    static ATTRIBUTES: OnceLock<Option<PipeSecurity>> = OnceLock::new();
    // The SECURITY_DESCRIPTOR behind the attributes is deliberately leaked: `CreateNamedPipeW`
    // reads it for the life of the process, the process is its only owner, and it is one small,
    // one-time allocation.
    ATTRIBUTES
        .get_or_init(build_security_attributes)
        .as_ref()
        .map(|security| &security.0)
}

fn create_instance(first: bool) -> std::io::Result<HANDLE> {
    // The first instance claims the name: if another helper already serves it, fail loudly
    // instead of silently running two servers and two injectors side by side.
    let open_mode = if first {
        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        PIPE_ACCESS_DUPLEX
    };
    let security = security_attributes();
    if security.is_none() {
        warn_default_security();
    }
    let handle = unsafe {
        CreateNamedPipeW(
            w!(r"\\.\pipe\BrightTray"),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            0,
            security.map(|sa| sa as *const SECURITY_ATTRIBUTES),
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

#[cfg(test)]
mod tests {
    //! The decisions the pipe server makes before it touches a monitor: the write rate, the
    //! command table and which monitors a target reaches. None of them needs a monitor, a pipe
    //! or a Windows handle — they are the logic that decides what happens, not the DDC/CI.

    use super::{
        parse_command, select_indices, Command, Target, WriteBudget, BUDGET_INTERVAL,
        WRITE_RATE_LIMIT,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn the_first_write_goes_out_immediately() {
        let t0 = Instant::now();
        let mut budget = WriteBudget::new(t0);
        assert!(budget.take(t0));
    }

    #[test]
    fn a_write_inside_the_budget_window_is_refused() {
        let t0 = Instant::now();
        let mut budget = WriteBudget::new(t0);
        assert!(budget.take(t0));
        // The budget holds one token, so the next write has to wait for it.
        assert!(!budget.take(t0 + Duration::from_millis(10)));
    }

    #[test]
    fn the_token_is_back_after_the_budget_interval() {
        let t0 = Instant::now();
        let mut budget = WriteBudget::new(t0);
        assert!(budget.take(t0));
        assert!(!budget.take(t0 + BUDGET_INTERVAL - Duration::from_millis(1)));
        assert!(budget.take(t0 + BUDGET_INTERVAL));
    }

    #[test]
    fn the_budget_reports_when_the_next_write_may_go_out() {
        let t0 = Instant::now();
        let mut budget = WriteBudget::new(t0);
        // An unused budget is ready right now.
        assert_eq!(budget.next_token(t0), t0);
        assert!(budget.take(t0));
        assert_eq!(budget.next_token(t0), t0 + BUDGET_INTERVAL);
    }

    #[test]
    fn a_flood_never_writes_more_than_the_rate_limit_per_second() {
        let t0 = Instant::now();
        let mut budget = WriteBudget::new(t0);
        let mut writes: Vec<Duration> = Vec::new();
        // A write request every millisecond for five seconds.
        for step in 0..5_000u64 {
            let at = t0 + Duration::from_millis(step);
            if budget.take(at) {
                writes.push(Duration::from_millis(step));
            }
        }
        // The cap holds over the whole flood: at most ten writes per second.
        assert!(writes.len() <= 5 * WRITE_RATE_LIMIT as usize);
        // ... and the budget does not starve the monitor either: the writes still go out.
        assert!(writes.len() >= 4 * WRITE_RATE_LIMIT as usize);
        // No one-second window holds more than WRITE_RATE_LIMIT writes. The window is half
        // open, so two writes exactly one second apart are not in the same window.
        for &start in &writes {
            let end = start + Duration::from_secs(1);
            let count = writes.iter().filter(|&&w| w >= start && w < end).count();
            assert!(
                count <= WRITE_RATE_LIMIT as usize,
                "{count} writes in the second starting at {start:?}"
            );
        }
    }

    /// The protocol table of the module header, one row per line.
    #[test]
    fn the_command_table_is_parsed_as_documented() {
        let cases: [(&str, Command); 13] = [
            ("list", Command::List),
            ("list extra", Command::Error),
            ("get 0", Command::Get(0)),
            ("get abc", Command::Error),
            ("get", Command::Error),
            ("get 0 1", Command::Error),
            ("getall", Command::GetAll),
            ("set 0 50", Command::Set(0, 50)),
            ("set 0 abc", Command::Dropped),
            ("set 0", Command::Error),
            ("setall 50", Command::SetAll(50)),
            ("setall abc", Command::Dropped),
            ("setall 50 60", Command::Error),
        ];
        for (line, expected) in cases {
            assert_eq!(parse_command(line), expected, "line: {line:?}");
        }
    }

    /// Only the first three words of a line are read, so a trailing word is ignored — as it
    /// has always been.
    #[test]
    fn a_trailing_word_is_ignored() {
        assert_eq!(parse_command("set 0 50 60"), Command::Set(0, 50));
    }

    /// `set` and `setall` never produce a reply, not even an `err`.
    #[test]
    fn writes_never_get_a_reply() {
        for line in ["set 0 50", "set 0 abc", "setall 50", "setall abc"] {
            assert!(!parse_command(line).replies(), "line: {line:?}");
        }
    }

    /// Everything that is not a write answers: the reads with their result, the rest with `err`.
    #[test]
    fn reads_and_errors_get_a_reply() {
        for line in [
            "list",
            "get 0",
            "getall",
            "list extra",
            "get abc",
            "get",
            "get 0 1",
            "set 0",
            "setall 50 60",
            "",
            "nonsense",
        ] {
            assert!(parse_command(line).replies(), "line: {line:?}");
        }
    }

    #[test]
    fn the_target_all_reaches_every_monitor() {
        let monitors = vec![
            ("key0".to_string(), "A".to_string()),
            ("key1".to_string(), "B".to_string()),
        ];
        assert_eq!(select_indices(&Target::All, &monitors), vec![0, 1]);
    }

    #[test]
    fn a_matching_key_reaches_that_monitor() {
        let monitors = vec![
            ("key0".to_string(), "A".to_string()),
            ("key1".to_string(), "B".to_string()),
        ];
        let target = Target::Monitor {
            key: "key1".to_string(),
            name: "B".to_string(),
        };
        assert_eq!(select_indices(&target, &monitors), vec![1]);
    }

    #[test]
    fn the_key_wins_over_the_name() {
        // Two monitors share the name; the key still picks exactly one of them.
        let monitors = vec![
            ("key0".to_string(), "Same".to_string()),
            ("key1".to_string(), "Same".to_string()),
        ];
        let target = Target::Monitor {
            key: "key1".to_string(),
            name: "Same".to_string(),
        };
        assert_eq!(select_indices(&target, &monitors), vec![1]);
    }

    #[test]
    fn the_name_is_the_fallback_when_the_key_is_gone() {
        let monitors = vec![
            ("key0".to_string(), "A".to_string()),
            ("key1".to_string(), "B".to_string()),
        ];
        // The display layout changed and the key is gone, but exactly one monitor has the name.
        let target = Target::Monitor {
            key: "gone".to_string(),
            name: "B".to_string(),
        };
        assert_eq!(select_indices(&target, &monitors), vec![1]);
    }

    #[test]
    fn an_ambiguous_name_reaches_nothing() {
        let monitors = vec![
            ("key0".to_string(), "Same".to_string()),
            ("key1".to_string(), "Same".to_string()),
        ];
        // Two monitors with that name: better no monitor than the wrong one.
        let target = Target::Monitor {
            key: "gone".to_string(),
            name: "Same".to_string(),
        };
        assert!(select_indices(&target, &monitors).is_empty());
    }
}
