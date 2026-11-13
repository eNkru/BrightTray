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
  `Monitor::brightness()` / `Monitor::set_brightness_percent()`; never call the
  dxva2 API directly. See [DDC/CI Access](./ddc-ci.md).
- **Release Windows handles.** Prefer the `windows` crate wrappers; close raw
  handles on every path, including errors.
- **Re-enumerate instead of guessing.** After a failed write, refresh the
  monitor list once and retry (`server.rs::flush`).
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

**The repository has no automated test suite** — there is no `tests/`
directory and no `#[cfg(test)]` module anywhere. Verification is manual plus
the compiler. Do not invent a test framework as a side effect of a feature;
if you add tests, keep them in the crate they belong to and make sure they
run on Windows.

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
- [ ] Writes are verified, not assumed to have landed.
- [ ] Handles are released on the error path too.
- [ ] No new `unwrap()` / `expect()` in library code.
- [ ] The module's `//!` docs still describe what the code does.
- [ ] The pipe protocol table in `README.md` and the one in `server.rs` agree.
- [ ] `cargo clippy` and `cargo fmt --check` are clean.
