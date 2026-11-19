# Quality Guidelines

## Required patterns

- **Validate, then patch.** Every slot address is checked against the module
  image, the table is dedupe-checked (`check_distinct`), the safe slots are
  probed (`probe_slot`), and the object is checked against the brightness id,
  before any write.
- **Pin the module** (`pin_self()`) before a vtable can point into it.
- **Catch panics** in every hooked entry point and return `E_FAIL`.
- **Fall back to the original** for every call the hook does not handle.
- **Ask for the identity on every call** — never cache it by address.
- **Keep `DllMain` trivial**: spawn a thread, sleep, install.
- **Do the work off the UI thread.** The `GetValue`/`SetValue` hooks run on
  ShellHost's threads and must return immediately.
- **Never set `panic = "abort"`.** The workspace release profile is in the
  root `Cargo.toml`; the hook's `catch_unwind` wrappers only work with the
  default `panic = "unwind"`.

## Forbidden patterns

| Forbidden | Why |
|-----------|-----|
| `unwrap()`, `expect()`, `panic!` anywhere in the injected code | `hook/src/lib.rs` currently has **zero** of them; a panic would unwind into ShellHost. `hook/build.rs` is a build script and may fail the build loudly - see "The DLL carries its identity" |
| Patching a slot whose pointer is outside the module | A Windows update would turn it into a jump into nowhere |
| Caching the brightness instance by address | A freed object's address can be reused by another tile |
| Doing DDC/CI in the DLL | Two DDC/CI speakers break the command gap; the helper owns the monitor handles |
| Blocking in a vtable hook | The shell calls these on its UI thread — a blocked call freezes Quick Settings |
| Doing work in `DllMain` | The loader lock is held; loading a module or calling COM there deadlocks the shell |
| A hard-coded RVA or offset | A system update moves it; the exported-function route survives |
| Removing a validation check to make an install succeed | Better a disabled hook than a crashed shell |
| `panic = "abort"` in the release profile | The hook relies on `catch_unwind` to survive a panic. With `panic = "abort"` there is no unwind, so a panic in a vtable hook would abort **ShellHost** instead of returning `E_FAIL` |

## The DLL carries its identity

`hook/build.rs` writes a `VERSIONINFO` resource into `OUT_DIR` and compiles it with
`embed-resource` (the same crate `app/build.rs` uses for the icon), so the DLL reports
`FileVersion` = `Cargo.toml`'s version padded to four numbers, and a `BuildId` string of
`BRIGHTTRAY_BUILD_ID` (CI sets the commit; local builds say `dev`).

The contract:

- **A change to hook behaviour or state bumps `hook/Cargo.toml`'s `version`.** Two
  content-different builds must not share a version. CI fails when the shipped DLL's
  `FileVersion` does not match the manifest, and when its `BuildId` is not the commit on a CI
  build, so a stale or hand-made DLL cannot pass as a release.
- **The resource is diagnostic identity, not a content proof.** It answers "which build is
  this?" for the log, Explorer's property sheet and a bug report. It does **not** replace the
  exact byte comparison in `app/src/hookdll.rs`: P1-07 measured that comparison at ~10 µs and
  rejected a content hash as ~18× slower, so parsing `.rsrc` to save those microseconds would
  trade a measured cost for a new misparse risk inside `ShellHost`.
- The `no unwrap/expect/panic!` rule above covers injected code. `hook/build.rs` is a build
  script: there, failing the build loudly is the correct response to a missing version, and it
  cannot run inside `ShellHost`.

## Testing

The hook crate carries inline `#[cfg(test)]` unit tests in `hook/src/lib.rs`:
`check_distinct` (the slot-table validation that runs before anything is
patched) and the log-rotation tests (`rotate_if_needed` / `append_line`, cap,
previous generation, missing file). They run under `cargo test --workspace
--all-targets` on the Windows CI runner; they are Windows-only binaries and do
not run on a Linux host.

The hook itself is still verified manually on a Windows machine with a DDC/CI
monitor — the unit tests do not load the DLL into ShellHost or touch a monitor.
See below.

## Verification steps

Build on Windows:

```powershell
cargo build --release
cargo clippy --workspace --all-targets
cargo fmt --check
```

`cargo build --release` produces `target/release/brighttray_hook.dll` and, via
`app/build.rs`, embeds it into `BrightTray.exe`.

Read-only diagnostic first — it never patches anything:

```powershell
target\release\brighttray-helper.exe probe
```

`probe` prints the `SettingsHandlers_PCDisplay` and `QuickActionsDataModel`
vtable slots, so you can see whether the slot layout still matches the
constants in `lib.rs`.

Then the live check:

1. `target\release\brighttray-helper.exe serve` (or start `BrightTray.exe`
   and press Apply).
2. Check `%TEMP%\brighttray-hook.log` for `IQuickAction hook installed`.
3. Open Quick Settings (<kbd>Win</kbd> + <kbd>A</kbd>) — the brightness slider
   must appear next to the volume slider.
4. Drag the slider — the external monitor's brightness must follow.
5. Change the brightness on the monitor's own OSD — the slider must follow
   back.
6. Check the other tiles (WiFi, Bluetooth, night light) — they must still show
   their own text and state. If they show a percentage, the identity filter
   broke.
7. Restart ShellHost (`taskkill /f /im ShellHost.exe`) — the watcher must
   re-inject, and the log must show it.

## Review checklist

- [ ] No `unwrap` / `expect` / `panic!` added to injected code (`hook/src/lib.rs` outside
  `#[cfg(test)]`); `hook/build.rs` is exempt, for the reason above.
- [ ] Every new vtable hook has a `catch_unwind` wrapper and an original-call
      fallback.
- [ ] Every new slot is validated with `in_module`, the table passes
      `check_distinct`, and the safe slots are probed — all before the first
      `VirtualProtect`.
- [ ] `DllMain` is still three lines.
- [ ] Nothing blocking happens on the caller's thread.
- [ ] The `//!` module docs in `lib.rs` still describe the behaviour.
- [ ] If a slot number changed, `docs/quick-settings-hook-research.md` was
      updated too.
