# P0-02 — Restrict the pipe and rate limit the writes

Audit item: **P0-02 命名管道加安全描述符 + 写速率上限**
Source: `/home/hju/sources/pt-monitor-win/artifacts/brighttray-fork-notes.md` §3

## Goal

`helper/src/server.rs` creates the pipe without a security descriptor:

```rust
CreateNamedPipeW(
    w!(r"\\.\pipe\BrightTray"),
    open_mode,
    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_UNLIMITED_INSTANCES,
    4096, 4096, 0,
    None,          // ← lpSecurityAttributes
)
```

`None` means the default security descriptor of the creating token. So **any
process running as the same user** can connect and issue `setall` commands,
which reach the monitor through DDC/CI. `PIPE_REJECT_REMOTE_CLIENTS` only
rejects *network* clients; it says nothing about other local processes.

There is also no rate limit: `handle_client()` processes every line it reads.

Two controls are needed:

1. **Who may connect.** The pipe belongs to the user who started BrightTray.
2. **How fast writes may reach the monitor.** A connected process must not be
   able to drive DDC/CI continuously.

## Requirements

### R1 — Only the current user may connect

The pipe is created with an explicit security descriptor whose access control
list grants full access to the **current user's SID** and to nobody else.

The descriptor is built from the running process's own token, not from a
hard-coded string, so it works for any account and any locale.

### R2 — A low-integrity process must not be able to write

The descriptor carries an explicit mandatory integrity label, so the
protection does not depend on the implicit default. The pipe is labelled
medium; a low-integrity process cannot write to it.

### R3 — The hook keeps working

The hook runs inside `ShellHost.exe` as the **same user** as the helper. It
must still be able to connect and to use `getall` / `setall`. The change must
not require a coordinated upgrade of the two sides.

### R4 — The pipe name and the wire format do not change

`\\.\pipe\BrightTray` stays, and the five commands keep their replies. A
helper that is already running in ShellHost must keep working against a new
helper.

### R5 — Monitor writes are capped per second

No monitor may be written more than `WRITE_RATE_LIMIT` times per second, no
matter which code path triggers the write — the normal write path, the
trailing write after a read, or the retry after a stale handle.

The cap is **per monitor**, so a legitimate multi-monitor setup is not
throttled by a global budget.

### R6 — A capped write is deferred, not lost

When the budget is exhausted, the value stays pending and is written as soon
as the budget allows. The user's requested brightness is never dropped.

### R7 — The server must never end up with a pipe nobody can connect to

If the security descriptor cannot be built, the server falls back to the
current behaviour and says so on stderr. It must not fail to start, and it
must not create a pipe with a broken descriptor.

## Constraints

- The change is in `helper/src/server.rs` (and the `windows` crate features in
  `helper/Cargo.toml`). The hook is unchanged.
- The token bucket builds on the write cadence from P0-04; the two tasks touch
  the same worker loop, so P0-04 lands first.
- No unit tests in this round.

## Acceptance criteria

- [ ] The pipe is created with an explicit `SECURITY_ATTRIBUTES`.
- [ ] The descriptor's DACL names the current user's SID, and nothing else.
- [ ] The descriptor carries a mandatory integrity label.
- [ ] **Windows check:** a process running as a **different user** cannot
      connect to `\\.\pipe\BrightTray`.
- [ ] **Windows check:** a **low-integrity** process cannot write to the pipe.
- [ ] **Windows check:** the hook still connects and still drives the slider
      — the normal flow is unchanged.
- [ ] **Windows check:** a flood of `setall` commands cannot make the monitor
      be written more than `WRITE_RATE_LIMIT` times per second.
- [ ] A capped write is not lost: the final value still lands.
- [ ] When the descriptor cannot be built, the server still starts and prints
      a warning.
- [ ] `cargo build --release`, `cargo clippy --workspace --all-targets` and
      `cargo fmt --check` are clean **on Windows**.
- [ ] The helper package spec describes the access control and the cap.

## Out of scope

- **A randomised pipe name with a discovery file.** The audit suggests it as
  well, but it breaks R3/R4 (a hook that is already loaded would not find the
  new name) and it adds a second source of truth for the name. The access
  control is the actual control; the name's predictability is not the
  problem.
- Restricting the *commands* per client (a client may only read, or only
  write).
- The audit's "token bucket of 10 writes/second" is adopted as a per-monitor
  budget; a global budget is not.
