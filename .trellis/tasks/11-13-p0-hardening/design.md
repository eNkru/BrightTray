# Design — P0 hardening

## Architecture context

Three crates, one process that owns the hardware, one that borrows the shell:

```
app (BrightTray.exe)          hook (brighttray_hook.dll)        helper (brighttray-helper.exe)
  ├─ owns window/page           ├─ lives inside ShellHost.exe     ├─ owns the monitor handles
  ├─ owns config.ini            ├─ patches 2 COM vtables          ├─ serves \\.\pipe\BrightTray
  └─ owns the Run key           └─ talks to the helper over it    └─ writes DDC/CI
```

The four P0 findings land on three different seams:

| Finding | Seam | Where the fix goes |
|---------|------|--------------------|
| P0-01 | app → helper (the `restart_stale` flag) | `helper/src/inject.rs`, `app/src/main.rs` |
| P0-03 | hook → shell (what the shell is told) | `hook/src/lib.rs` |
| P0-04 | helper → monitor (write cadence) | `helper/src/ddc.rs`, `helper/src/server.rs` |
| P0-02 | any local process → helper (pipe + write rate) | `helper/src/server.rs` |

Two of them (P0-04, P0-02) change the same worker loop in `server.rs`. That
is why they are sequenced.

## Shared design decisions

### D1 — "Known or unknown", never "assumed"

P0-03 and P0-04 both come from the same root cause: the code cannot tell the
difference between *a value it measured* and *a value it made up*. The
convention for this round is therefore explicit:

> Every value that leaves the helper or the hook is either measured or absent.
> A default is never a substitute for a measurement.

Concretely: the hook's cached percent becomes "known / unknown" instead of a
number with a fallback, and the server's write path stops inventing a
verification that was never performed.

### D2 — The write path gets one owner and one clock

Today the DDC/CI write rate is decided implicitly by "how fast the worker
can loop". After this round there are exactly two rate controls, and both
live in the helper:

| Control | Scope | Mechanism | Set by |
|---------|-------|-----------|--------|
| Write throttle | per monitor | minimum interval between two writes to the same monitor | P0-04 |
| Token bucket | global | maximum number of monitor writes per second | P0-02 |

The throttle bounds the wear; the bucket bounds the exposure from a hostile
same-user process. The throttle is the normal path; the bucket is the
backstop that also covers the paths that bypass the throttle.

The hook never writes to a monitor, so neither control applies to it.

### D3 — Timing changes stay in the helper

The hook polls the helper every 1.5 s and skips the poll for 1.2 s after a
UI-originated write. If the helper defers a write by up to the throttle
interval, the hook's cached value lags by that much — which the existing
1.2 s window already covers. No change to the hook's timing is needed, and
none is planned.

### D4 — Fail safe, always

The hook's rules are unchanged and remain absolute: validate before patching,
catch every panic, never block the caller's thread. None of the four changes
touches a vtable slot or a COM call.

### D5 — The pipe name and the wire format are frozen

The pipe keeps the name `\\.\pipe\BrightTray` and the five commands with
their current replies. The hook in an already-running ShellHost must keep
working against a new helper. A randomised name with a discovery file would
break that, so it is not part of this round; the access control is the
security descriptor instead.

## Verification model

There is no Rust toolchain in the environment where this code is written, and
the `windows` crate does not build for non-Windows targets. **No compile check
is possible while the code is written.** The consequences are built into the
plan:

1. Every child task ends with a *Windows* verification gate:
   `cargo build --release`, `cargo clippy --workspace --all-targets`,
   `cargo fmt --check`, plus the manual steps for that task.
2. Changes are kept small and reviewable, so that a compile error on Windows
   is a typo rather than a design problem.
3. Anything that needs a Windows runtime check is written down as an explicit
   step with an expected observation, not as "it should work".

The audit's §5 checklist is the shared source for the manual steps.

## Risk register

| Risk | Mitigation |
|------|------------|
| Deferring the write breaks the "slider follows the finger" feel | The throttle is a *minimum interval*, not a debounce: the latest value is written at most every 300 ms and always within 300 ms of the drag stopping. The trailing write carries the final value. |
| The hook hides the tile when the helper is down | Accepted: a missing tile is honest, a wrong number is not. The tile comes back when the helper returns and the panel is rebuilt. |
| A wrong security descriptor locks the hook out of the pipe | The descriptor is built and verified before the first `CreateNamedPipeW`; if the SDDL cannot be built, the server must fall back to the current behaviour and log it — never to a pipe nobody can connect to. |
| The token bucket drops the user's last value | A dropped write stays *pending*; the next tick retries it. Only the excess is discarded. |

## Rollback

Every change is local to one file cluster and can be reverted independently:

| Task | Files | Revert |
|------|-------|--------|
| P0-01 | `helper/src/inject.rs`, `app/src/main.rs` | restore `watch_and_inject(dll, true, …)` |
| P0-03 | `hook/src/lib.rs` | restore the `AtomicU32` + fallback constant |
| P0-04 | `helper/src/ddc.rs`, `helper/src/server.rs` | restore the verified write in `flush()` |
| P0-02 | `helper/src/server.rs`, `helper/Cargo.toml` | restore `SECURITY_ATTRIBUTES = None` and drop the bucket |

No data format, no file layout and no public API changes are involved, so no
migration is needed.
