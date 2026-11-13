# Quality Guidelines

## Required patterns

- **Validate, then patch.** Every slot address is checked against the module
  image, and the object is checked against the brightness id, before any write.
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
| `unwrap()`, `expect()`, `panic!` anywhere in the crate | The crate currently has **zero** of them; a panic would unwind into ShellHost |
| Patching a slot whose pointer is outside the module | A Windows update would turn it into a jump into nowhere |
| Caching the brightness instance by address | A freed object's address can be reused by another tile |
| Doing DDC/CI in the DLL | Two DDC/CI speakers break the command gap; the helper owns the monitor handles |
| Blocking in a vtable hook | The shell calls these on its UI thread — a blocked call freezes Quick Settings |
| Doing work in `DllMain` | The loader lock is held; loading a module or calling COM there deadlocks the shell |
| A hard-coded RVA or offset | A system update moves it; the exported-function route survives |
| Removing a validation check to make an install succeed | Better a disabled hook than a crashed shell |
| `panic = "abort"` in the release profile | The hook relies on `catch_unwind` to survive a panic. With `panic = "abort"` there is no unwind, so a panic in a vtable hook would abort **ShellHost** instead of returning `E_FAIL` |

## Testing

There is **no automated test suite** in this repository. The hook is verified
manually on a Windows machine with a DDC/CI monitor — see below.

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

- [ ] No `unwrap` / `expect` / `panic!` added.
- [ ] Every new vtable hook has a `catch_unwind` wrapper and an original-call
      fallback.
- [ ] Every new slot is validated with `in_module` before it is written.
- [ ] `DllMain` is still three lines.
- [ ] Nothing blocking happens on the caller's thread.
- [ ] The `//!` module docs in `lib.rs` still describe the behaviour.
- [ ] If a slot number changed, `docs/quick-settings-hook-research.md` was
      updated too.
