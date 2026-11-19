# Pipe Protocol — `\\.\pipe\BrightTray`

The helper serves one named pipe; the injected hook is the client. The app
talks to the *library* (`server::Server`) instead of the pipe, so it never
appears as a client — with one exception: `helper/src/client.rs`, which the
app's brightness-key handler uses for `getall` / `setall`. That module blocks
(its callers are app threads, never a shell UI thread) and deliberately does
not share code with the hook's non-blocking copy.

The authoritative implementation is `helper/src/server.rs`; the clients are
`hook/src/pipe.rs` (see the hook package's
[Pipe Client](../../brighttray-hook/backend/pipe-client.md)) and
`helper/src/client.rs`.

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

The three read commands force any throttled write out before they read, so a
read reports the value that was last *requested*, not the one that happened to
reach the monitor yet (see [the write cadence](#coalescing-and-the-write-cadence)).

The same table is documented in `README.md` under "Pipe protocol" — keep the two
in sync.

## Who may connect

**Only the current user can connect; a low-integrity process cannot write.**

`CreateNamedPipeW` is called with an explicit `SECURITY_ATTRIBUTES`, built once
from the running process's own token and kept for the lifetime of the process.
The DACL is `D:P(A;;GA;;;<user SID>)`: generic all for the current user's SID
and nobody else — no ACE for SYSTEM or the administrators group.
`PIPE_REJECT_REMOTE_CLIENTS` still rejects *remote* (network) clients; the DACL
is what keeps other local accounts out.

The descriptor also carries an explicit mandatory label `S:(ML;;NW;;;ME)`:
medium integrity, no write-up. A low-integrity process cannot write to the
pipe, and the protection does not depend on the implicit default.

The descriptor is built from the process's own token (`OpenProcessToken` →
`GetTokenInformation(TokenUser)` → `ConvertSidToStringSidW`), never from a
hard-coded string, so it is correct for any account and locale. If it cannot
be built, the server falls back to the default descriptor and prints one
warning — it never fails to start, and it never creates a pipe nobody can
connect to.

There is also a **rate limit on writes**: no monitor is written more than
`WRITE_RATE_LIMIT` (10) times per second, whichever path asks for the write —
the normal write path, the write a read forces out, the trailing read-back
resend, or the stale-handle retry. The cap is **per monitor**, so a
multi-monitor setup is not throttled by a global budget. A write that would
exceed the cap is deferred, never dropped: the value stays pending and is
written as soon as the budget allows. The stale-handle retry takes a token of
its own, like every other physical write: it is a second write to the
monitor, and the cap counts writes, not logical values.

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
batches (`batch.extend(rx.try_iter())`) so a burst becomes one pass, and paces
the resulting writes (next section).

## Coalescing and the write cadence

`pending: BTreeMap<usize, u32>` holds one value per monitor, **last value
wins**. The BTreeMap keeps the writes in monitor order, which is what a stable
DDC/CI bus wants.

A drag delivers its steps spread over time, so coalescing one batch is not
enough: every write is an EEPROM write on the monitor. The worker therefore
paces them itself:

| State | Meaning |
|-------|---------|
| `pending` | the newest value that still has to be written |
| `throttle` | when each monitor was last written to (`WRITE_INTERVAL` = 300 ms) |
| `verify` | the value that was written, and when its read-back is due (`VERIFY_DELAY` = 500 ms) |
| `budgets` | the per-monitor write budget, `WRITE_RATE_LIMIT` (10) writes per second |

The worker sleeps in `rx.recv_timeout()` until the earliest of those deadlines
(`next_deadline()`) and blocks in `rx.recv()` when nothing is scheduled.

- `write_due()` — writes every pending value whose 300 ms window has passed.
  The first write after a quiet period has no window and goes out immediately;
  during a drag the pending entry always holds the newest value, so a
  throttled write is late but never stale.
- `verify_due()` — reads back 500 ms after the last write and resends once if
  the monitor answered with a different value. That resend is not verified
  again and counts as a write, so it opens the same 300 ms window. A drag
  therefore costs one read and at most one resend, not one of each per step.
- `flush()` — the force variant, called by `get`, `getall` and `list`: the
  pending value is written out **now**, ignoring the window, because a read has
  to answer with the value that was last requested. The forced write schedules
  the same trailing read-back as any other write, so a read cannot leave a
  write unverified.

Every write takes a token from that monitor's bucket first, whichever path asks
for it. The bucket holds **one** token that refills `WRITE_RATE_LIMIT` times
per second, so two writes to the same monitor are always at least
`BUDGET_INTERVAL` (100 ms) apart and no one-second window ever holds more than
`WRITE_RATE_LIMIT` writes — not even in a burst. When the token is gone the
value stays in `pending` (or, for a read-back resend, in `verify`) and is
written on a later tick — deferred, never dropped.

Both write paths keep the stale-handle rule: a write that could not be sent
re-enumerates the monitors once and retries. Every re-enumeration goes through
`reenumerate()`, which re-keys `pending`, `throttle`, `verify` and `budgets`
by `Monitor::key()`: an index only means something for one enumeration. A
cadence entry whose monitor disappeared is dropped, not re-enumerated — there
is no handle left to write to or read back from.

So a `get` normally reports the state after the pending writes, and a burst of
100 `setall` lines costs two writes and one read per monitor, not 100 of each.
During a flood the bucket can defer a read-forced write, in which case the read
reports the monitor's current value and the pending value still lands once the
budget refills — the cap wins over read freshness.

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

Two consequences, both worth knowing on a laptop:

- The command is **`Cmd::SetAll`**: every monitor the current
  [target](#the-target) covers. With `Target::All` (the CLI) that is every
  DDC/CI-capable monitor; with `Target::Monitor` (the GUI, after Apply) it is
  the one display the user applied. Neither is "whichever display the user was
  dimming in Settings".
- A DDC/CI brightness write is an **EEPROM write**, and a drag keeps producing
  them. The worker paces them ([coalescing](#coalescing)) to at most one write
  per `WRITE_INTERVAL` (300 ms), so a forty-step drag is a handful of writes,
  not forty - but every one of them lands on a display the user did not mean to
  change.

So on a laptop, where Windows owns the internal panel and shows its brightness
slider, every drag of that slider also moves the external monitors. The escape
hatch is `BRIGHTTRAY_NO_MIRROR`: with it set to `1`, `true`, `yes` or `on`
(trimmed, case-insensitive), `Server::start` does not subscribe at all — no
power-manager callback runs and no DDC/CI work is queued. Any other value, the
empty string and an unset variable mean the mirroring is on, which is the
**default**. The switch sits inside `Server::start`, so the GUI and
`brighttray-helper serve` honour it identically; set the variable in the
environment BrightTray is started from (`set BRIGHTTRAY_NO_MIRROR=1` before
launching it, or `setx` for a persistent setting).

The switch covers the power-manager path only. The Quick Settings slider
BrightTray installs is the hook's (`setall` through the pipe, an independent
client) and keeps working, as do the app's brightness keys. That is the point:
the escape hatch is for a slider the user did not aim at BrightTray. Note that
`brighttray-helper watch` still subscribes - it prints values and writes
nothing, so it is a diagnostic, not a second mirroring path.

`BRIGHTTRAY_NO_MIRROR` is value-parsed, unlike `BRIGHTTRAY_NO_HOOK` which is
presence-based: `BRIGHTTRAY_NO_MIRROR=0` keeps the mirroring on purpose, so a
value copied from a script cannot silently turn the feature off.

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
  value it had. Before the first successful read the hook has no value at all
  and tells the shell nothing — it falls back to the system's own brightness
  behaviour instead of inventing one. Nothing crashes, nothing is logged per
  poll.
- **Bad** — the user sends `set 0 200`: the percent is written to the monitor
  as 100 % (`percent.min(100)` in `scale_percent`), and the reply is still
  absent.

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
