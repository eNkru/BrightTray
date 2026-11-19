# Pipe Client

The hook is the only pipe client. The server is `helper/src/server.rs`; the
protocol is in the helper package's
[Pipe Protocol](../../brighttray-helper/backend/pipe-protocol.md). The client
code lives in `hook/src/pipe.rs`.

## The helper is the only DDC/CI speaker

> The hook runs inside ShellHost, so it must never keep a monitor handle of its
> own: the helper is the only process that talks DDC/CI.

The hook asks the helper for a brightness and hands it new values. It never
opens a `PHYSICAL_MONITOR`, never calls `SetVCPFeature`, and never sleeps to
respect the DDC command gap. Those rules belong to the helper
([DDC/CI Access](../../brighttray-helper/backend/ddc-ci.md)).

## Every call is best-effort

The helper may not be running (the user may have closed BrightTray, or the
pipe may be taken by another instance). So:

- `get_all()` returns `Option<u32>`; `None` means "could not ask", not
  "brightness is unknown" and not an error.
- `set_all(percent)` returns nothing and reports nothing.
- No function in `pipe.rs` logs, panics or retries forever.

## Reads block, writes must not

`get_all()` waits for the helper and reads a line — it blocks. It is called
from `install()` (once, on the install thread) and from the background watcher
thread (`start_watcher` in `lib.rs`), which sleeps 1.5 s between polls. Never
from a vtable hook.

`set_all()` is called from `SetValue`, which the shell invokes on **its UI
thread while the slider is being dragged**. It must never block, so it hands
the value to a writer thread:

```rust
pub fn set_all(percent: u32) {
    static SENDER: OnceLock<Sender<u32>> = OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (tx, rx) = channel();
        std::thread::spawn(move || writer(rx));
        tx
    });
    let _ = sender.send(percent.min(100));
}
```

The `OnceLock` means one writer thread and one open connection for the life of
the process, no matter how often the slider moves.

## Collapse a burst into the last value

The writer drains the channel and keeps only the newest value:

```rust
while let Ok(mut percent) = rx.recv() {
    // Collapse a burst of slider steps into the last one.
    for newer in rx.try_iter() {
        percent = newer;
    }
```

A drag produces dozens of `SetValue` calls; only the final one matters. This
mirrors the server-side coalescing in `pending: BTreeMap<usize, u32>`.

## Connecting

```rust
const PIPE_NAME: &str = r"\\.\pipe\BrightTray";
const WAIT_MS: u32 = 250;
```

`wait_for_pipe()` calls `WaitNamedPipeW` with a 250 ms timeout and ignores the
result, then `connect()` opens the pipe read/write. The short timeout keeps a
missing helper from stalling the caller.

The writer reconnects once per value (two attempts) because the helper may
have restarted since the last write:

```rust
for _ in 0..2 {
    if connection.is_none() {
        connection = connect();
    }
    ...
}
```

## The commands used

| Call | Command | Reply |
|------|---------|-------|
| `get_all()` | `getall` | `percent\n` or `err\n` |
| `set_all(p)` | `setall {percent}` | none |

The client parses the reply with `line.trim().parse::<u32>().ok().map(|p| p.min(100))`
— an unparsable reply is treated as "no answer", not as an error.

The hook never uses `list`, `get` or `set`: it has no notion of an individual
monitor. The helper decides which monitors a `setall` reaches
(`server::Target`).
