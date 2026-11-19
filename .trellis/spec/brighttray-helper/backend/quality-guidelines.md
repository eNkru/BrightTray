# Quality Guidelines

## The helper never terminates ShellHost on its own

Two paths in `helper/src/inject.rs` end a ShellHost process with
`TerminateProcess`, and both need the user's consent:

- `inject::restart_shellhost()` ends every running ShellHost. The app's tray
  menu has an item that calls it, so the user asks for it by clicking.
- `inject::inject_early()` ends every running ShellHost before starting a
  suspended replacement. The CLI gates it behind `--force-restart`.

`inject::watch_and_inject` **never** ends a process. It only injects into a
ShellHost that is running and does not have the hook yet, and it does so
whether the shell is five seconds or five hours old. Injecting into a
running ShellHost is enough, because the `IQuickAction` hook answers every
later query.

The reason the restart exists at all is that Quick Settings caches which tiles
are applicable when it builds its panel; a hook that arrives later than that
may be too late for the brightness slider to appear. That is a fallback for a
few Windows builds, not the normal path.

Killing the shell is visible to the user: the Start menu and Quick Settings
disappear for a moment. Treat it as something that needs an explicit reason,
never as a default.

## Required patterns

- **Pace and verify every DDC/CI access.** Go through
  `Monitor::brightness()`, `Monitor::write_brightness_percent()`,
  `Monitor::verify_brightness()` / `Monitor::set_brightness_percent()`; never
  call the dxva2 API directly. See [DDC/CI Access](./ddc-ci.md).
- **Release Windows handles.** Prefer the `windows` crate wrappers; close raw
  handles on every path, including errors.
- **Re-enumerate instead of guessing.** After a failed write, refresh the
  monitor list once and retry (`server.rs::write_values`).
- **Bound the write rate, not the write latency.** A drag writes at most once
  per `WRITE_INTERVAL` and is verified once at the end; never verify a write
  that is only one step of a drag.
- **Keep the CLI thin.** `helper/src/main.rs` parses arguments and prints;
  the behaviour lives in the library.
- **Document the *why*.** Every module opens with a `//!` block that explains
  the problem it solves and the constraint it works under (see
  `ddc.rs`, `server.rs`, `inject.rs`, `slider.rs`, `probe.rs`). Public items
  carry `///` docs. This is the house style — a new module without a rationale
  header reads as unfinished.
- **Comments explain the constraint, not the syntax.** The existing comments
  explain *why* 50 ms, *why* the first slider callback is ignored, *why* the
  DllMain stays tiny.

## Forbidden patterns

| Forbidden | Why |
|-----------|-----|
| `unwrap()` / `expect()` in library code | A panic in the helper kills the pipe server that the hook and the app depend on |
| Calling `SetVCPFeature` / `GetVCPFeatureAndVCPFeatureReply` outside `ddc.rs` | The pacing, retry and verification logic lives with those calls |
| Opening a monitor handle in another process | The hook runs inside ShellHost; a second DDC/CI speaker breaks the command gap |
| Hard-coding a vtable slot, RVA or offset in the helper | The helper has no vtable code on purpose; the hook validates every slot at runtime |
| Adding a logging or argument-parsing crate | The project deliberately has neither; see [Logging Guidelines](./logging-guidelines.md) |
| Swallowing an error without a reason | The error text is the only diagnostic the user gets |
| Blocking the pipe worker | A slow DDC/CI call must not stall other clients; the worker batches commands instead |
| Ending a ShellHost process from an automatic path | The Start menu and Quick Settings disappear while it is down; only the tray menu item and `--early --force-restart` may do it |

## Testing

The tests are unit tests inside the crate that owns the logic: an inline
`#[cfg(test)] mod tests` at the end of the module. There is no `tests/`
directory and no test framework — do not invent one as a side effect of a
feature. The CI runs `cargo test --workspace --all-targets` on a Windows
runner, so a test that fails turns the build red.

### What is testable, and how

Only the **decisions** are tested; the Windows calls are not. A decision that
is currently buried in an API call is pulled one level up into a free
function, and the API call keeps calling it:

```rust
/// The native VCP value for `percent` of a monitor whose brightness range ends at `max`.
fn scale_percent(percent: u32, max: u32) -> u32 {
    (percent.min(100) * max + 50) / 100
}
```

The rule: **extract, do not rewrite**. The behaviour stays identical, and the
diff reads as "the same code, one level higher". A test that needs a
monitor, a handle, a pipe or a shell is not a unit test — it belongs in the
manual verification list below.

What the tests cover today:

- `ddc.rs` — the scaling arithmetic: the rounding, the 100 % clamp, a monitor
  without a range.
- `server.rs` — the write budget (at most `WRITE_RATE_LIMIT` writes per
  second per monitor, a refused write is deferred), the command table of the
  pipe protocol (including the `set`/`setall` asymmetry), and the target
  selection.
- `inject.rs` — the resolved system directory is absolute, exists and is named
  System32, and `ShellHost.exe` is looked up inside it.
- `app/src/config.rs` — the `key=value` settings file.
- `app/src/brightness_keys.rs` — the HID report parser and the ±10 step with
  its 0/100 clamps.

`helper/src/client.rs` has no tests on purpose: it is a pipe round trip, and a
unit test would need a server. Its behaviour is covered by the manual
verification list in the app package.

Drive time-dependent code with explicit instants, never with sleeps:

```rust
let t0 = Instant::now();
assert!(budget.take(t0));                        // the first write goes out
assert!(!budget.take(t0 + Duration::from_millis(10)));  // the budget is empty
```

The crates are Windows-only, so the tests run on Windows — that is where the
CI runs them.

### A test may call Windows, but must not depend on one machine

`inject.rs` resolves the system directory through `GetSystemDirectoryW` and
asserts on the answer. Two rules kept that test honest on the CI runner
(a Server image) rather than on a desktop:

- **Never compare a Windows path to a literal, case included.**
  `GetSystemDirectoryW` returns the casing the volume recorded — `system32` on
the runner, `System32` on a desktop. Compare with
  `name.eq_ignore_ascii_case("System32")`, not `assert_eq!`.
- **Assert on what Windows always answers, not on what a desktop happens to
  have.** The directory must exist and must be named System32; the shell host
  inside it must not be required to exist, because the runner does not ship
  one.

A test that passes locally and is red on the runner costs a merge and a
follow-up PR: the gate is the runner, so that is the machine the assertions
are written for.

## Verification steps

Build and lint on Windows (the `windows` crate does not build elsewhere):

```powershell
cargo build --release
cargo clippy --workspace --all-targets
cargo fmt --check
```

`cargo build --release` also builds the hook through `app/build.rs` and embeds
it into `BrightTray.exe`.

Manual checks that matter for a change in this crate:

```powershell
target\release\brighttray-helper.exe list      # enumeration and brightness read
target\release\brighttray-helper.exe set 0 50  # write + verify
target\release\brighttray-helper.exe probe     # read-only, safe on a live machine
```

`probe` never patches anything, so it is always safe to run.

## Review checklist

- [ ] Every DDC/CI call goes through `ddc::Monitor`.
- [ ] Writes are verified, not assumed to have landed — once per drag in the
      server, on the single write in the CLI.
- [ ] Handles are released on the error path too.
- [ ] No new `unwrap()` / `expect()` in library code.
- [ ] The module's `//!` docs still describe what the code does.
- [ ] The pipe protocol table in `README.md` and the one in `server.rs` agree.
- [ ] `cargo clippy` and `cargo fmt --check` are clean.
