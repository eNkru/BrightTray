# Pipe Protocol — `\\.\pipe\BrightTray`

The helper serves one named pipe; the injected hook is the client. The app
talks to the *library* (`server::Server`) instead of the pipe, so it never
appears as a client.

The authoritative implementation is `helper/src/server.rs`; the client is
`hook/src/pipe.rs` (see the hook package's
[Pipe Client](../../brighttray-hook/backend/pipe-client.md)).

## Wire format

Byte-mode pipe, **local connections only** (`PIPE_REJECT_REMOTE_CLIENTS`), one
command per line, ASCII.

| Command | Reply |
|---------|-------|
| `list` | one line per monitor `index\tcurrent\tmax\tname`, then an empty line |
| `get <index>` | `current max`, or `err` |
| `set <index> <0-100>` | none |
| `getall` | brightness percent of the first targeted DDC/CI-capable monitor, or `err` |
| `setall <0-100>` | none |

`set` and `setall` deliberately have **no reply**. The hook's writer thread
sends a value on every slider step; making the write acknowledge would turn
the drag into a request/response round trip on ShellHost's UI thread. `err` is
the only failure reply, and it means "could not do that right now", not
"something broke".

The same table is documented in `README.md` under "Pipe protocol" — keep the two
in sync.

## Who may connect

**Any process running as the same user can connect and can write.**

`CreateNamedPipeW` is called with `lpSecurityAttributes = None`, so the pipe
gets the default security descriptor of the creating token. `PIPE_REJECT_REMOTE_CLIENTS`
rejects *remote* (network) clients — it does nothing against other local
processes running as the same user, which includes any application the user
starts.

There is also **no rate limit**: `handle_client()` processes every line it
reads, and `setall` is a write that reaches the monitor. A local process that
connects in a loop can drive DDC/CI continuously.

These two facts are the current state of the code, not a recommendation. They
matter when you touch the server: a new command that writes to a monitor
multiplies the exposure, and a change to the pipe name or its security
descriptor changes what other processes can reach.

## Who owns the name

The first server claims the pipe with `FILE_FLAG_FIRST_PIPE_INSTANCE`. If
another BrightTray already owns it, `CreateNamedPipeW` fails with
`ERROR_ACCESS_DENIED`, which `create_instance()` maps to:

```rust
Err(std::io::Error::new(
    std::io::ErrorKind::AddrInUse,
    r"\\.\pipe\BrightTray is already served by another BrightTray instance",
))
```

`AddrInUse` is the signal the app turns into "Another BrightTray is already
running, close it first" (`app/src/main.rs`). Do not silently steal the name.

## Threading model

Three kinds of threads, and the split matters:

- **accept loop** (`accept_loop`) — one thread, creates the next pipe instance
  and hands every connected client to its own thread.
- **client threads** (`handle_client`) — one per client, parse lines, push
  commands into the channel, write the reply.
- **worker** (`worker`) — exactly one thread, owns the `Vec<ddc::Monitor>` and
  performs *all* DDC/CI.

The worker owns the monitor handles because DDC/CI is slow (~50 ms per command)
and a monitor handle is not safe to use from two threads at once. Commands
arrive over an `mpsc` channel of `Cmd` values; the worker drains the channel in
batches (`batch.extend(rx.try_iter())`) so a burst becomes one pass.

## Coalescing

While a write is in flight, further writes are collected in
`pending: BTreeMap<usize, u32>` — one entry per monitor, **last value wins**.
The BTreeMap keeps the writes in monitor order, which is what a stable DDC/CI
bus wants.

`flush()` is called after every batch and before every read, so a `get` always
reports the state after the pending writes.

## The target

`getall` / `setall` act on `server::Target`:

- `Target::All` — every monitor. This is what `brighttray-helper serve` uses.
- `Target::Monitor { key, name }` — one monitor. This is what the GUI uses
  after the user applies a display.

The app switches the target at runtime with `Server::set_target()`, which
sends `Cmd::SetTarget` to the worker; the change applies to the next command.

## The Windows slider is mirrored

While serving, the server subscribes to the Windows brightness slider
(`slider::subscribe`) and forwards real moves to `Cmd::SetAll`. The first
callback is the slider's *current* position delivered on registration, so the
server ignores it — starting the server must not change any monitor:

```rust
if last.is_some_and(|l| l != p) {
    let _ = tx.send(Cmd::SetAll(p));
}
*last = Some(p);
```

## Validation and error matrix

| Input line | What the server does | Reply |
|------------|----------------------|-------|
| `list` | queues `Cmd::List` | one `index\tcurrent\tmax\tname` line per monitor, then an empty line |
| `list extra` | argument count does not match | `err` |
| `get 0` | queues `Cmd::Get(0)` | `current max`, or `err` if the monitor does not answer |
| `get 99` | queues `Cmd::Get(99)` | `err` (no monitor with that index) |
| `get abc` | the index does not parse | `err` |
| `get` | argument count does not match | `err` |
| `set 0 50` | queues `Cmd::Set(0, 50)` | **none** |
| `set 0 abc` | the value does not parse — the command is dropped | **none** |
| `set 0` | argument count does not match | `err` |
| `setall 50` | queues `Cmd::SetAll(50)` | **none** |
| `setall abc` | the value does not parse — the command is dropped | **none** |
| `getall` | queues `Cmd::GetAll` | `percent`, or `err` |
| anything else | not recognized | `err` |

Note the asymmetry: **`set` and `setall` never produce a reply**, not even
`err`. The parser `continue`s before the reply is written, so a malformed
write is silent by design — the client is the shell and must not wait for an
acknowledgement.

## Good / base / bad cases

- **Good** — the hook drags the slider to 40: `setall 40` → the helper writes
  40 % to the targeted monitor, the slider shows 40 %.
- **Base** — the helper is not running: the hook's `get_all()` returns `None`,
  the watcher keeps the last cached percent, and the shell shows the last
  value it had. Nothing crashes, nothing is logged per poll.
- **Bad** — the user sends `set 0 200`: the percent is written to the monitor
  as 100 % (`percent.min(100)` in `set_brightness_percent`), and the reply is
  still absent.

## Wrong vs correct

```rust
// Wrong: every command gets a reply, so a slider drag becomes a
// request/response round trip on ShellHost's UI thread.
let reply = match command { ... };
out.write_all(reply.as_bytes())?;
```

```rust
// Correct: `set` and `setall` continue before the reply is written.
(Some("setall"), Some(p), None) => {
    if let Ok(p) = p.parse() {
        let _ = tx.send(Cmd::SetAll(p));
    }
    continue;
}
```

## Adding a command

1. Add a `Cmd` variant in `server.rs`.
2. Handle it in `worker()`. Reply values are sent through a `Sender<String>`
   that is part of the variant.
3. Add the parsing arm in `handle_client()`. The arm must match the exact
   arity — the parser destructures `(parts.next(), parts.next(), parts.next())`
   and falls through to `err` for anything else.
4. Document it in the module header of `server.rs` and in the README table.
