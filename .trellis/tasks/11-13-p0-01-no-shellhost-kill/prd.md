# P0-01 — Stop terminating ShellHost for injection

Audit item: **P0-01 不要为了注入而杀掉 ShellHost**
Source: `/home/hju/sources/pt-monitor-win/artifacts/brighttray-fork-notes.md` §3

## Goal

Nothing in BrightTray's normal flow may end a `ShellHost.exe` process. Today
two paths do:

- `helper/src/inject.rs` — `watch_and_inject(dll, restart_stale = true, …)`
  kills a ShellHost that has been alive for more than 10 s (at most once per
  60 s).
- `helper/src/inject.rs` — `inject_early()` kills **every** running ShellHost
  before starting a suspended replacement.

The app passes `restart_stale = true` (`app/src/main.rs:248`), so pressing
**Apply** in the tray app can make the Start menu and Quick Settings vanish
for a second. The user sees Windows itself break.

The restart capability must survive, but only as an explicit, user-visible
action.

## Requirements

### R1 — The watcher never terminates a process

`inject::watch_and_inject` must not contain a code path that calls
`TerminateProcess`. The `restart_stale` parameter and the stale-restart
branch are removed, so the dangerous path cannot be reached by accident.

### R2 — The app does not restart the shell

`app/src/main.rs` must not ask for a shell restart. The app's watcher only
injects into a ShellHost that is running and does not have the hook yet.

### R3 — The user can still force a restart, deliberately

The app's tray menu gets an explicit item that restarts ShellHost. Clicking it
is the consent: the item's label states the consequence. The app then waits
for `sihost` to bring ShellHost back and the existing watcher injects into
the new process — the same path that already works after an unexpected
ShellHost restart.

### R4 — The CLI keeps an explicit early-inject path, behind a second flag

`brighttray-helper inject --early` terminates every ShellHost. It must now
require a second, unambiguous flag:

```
brighttray-helper inject --early --force-restart [dll]
```

`inject --early` on its own prints an explanation and exits with the usage
exit code (2). It must not terminate anything.

### R5 — The dead code and the dead strings go

- `WatchEvent::Restarting` disappears with the branch that produced it, along
  with its `Display` arm, the `push_state()` mapping and the page's
  `statusOf()` arm and its `restarting` string.
- `FRESH_SHELLHOST`, `RESTART_COOLDOWN` and `process_age()` disappear with the
  stale branch, unless `process_age()` is still used elsewhere (it is not).

## Constraints

- The pipe protocol, the hook and the DDC/CI paths are untouched.
- The shell must still get the hook in the two scenarios the audit names:
  a cold start, and a ShellHost that has been alive for an hour.
- The early-inject path itself keeps working — it is the fallback for builds
  where Quick Settings caches the brightness capability before a late hook can
  take effect. Only its *reachability* changes.
- No unit tests in this round.

## Acceptance criteria

- [ ] `grep -n "TerminateProcess" helper/src/inject.rs` shows the calls only
      in the explicit restart helper and in `inject_early`.
- [ ] No code path in `watch_and_inject` reaches `TerminateProcess`.
- [ ] `app/src/main.rs` starts the watcher without asking for a restart.
- [ ] **Windows check:** start `BrightTray.exe`, note the ShellHost pid, press
      **Apply**, and confirm the pid is unchanged afterwards and during the
      apply.
- [ ] **Windows check:** with ShellHost alive for an hour, the Quick Settings
      brightness slider still appears after an apply.
- [ ] **Windows check:** with a cold-started ShellHost, the slider still
      appears.
- [ ] The tray menu item restarts ShellHost on click, and the hook is injected
      into the new process.
- [ ] `brighttray-helper inject --early` (without `--force-restart`) prints an
      explanation, terminates nothing, and exits with code 2.
- [ ] `brighttray-helper inject --early --force-restart` still works.
- [ ] `WatchEvent::Restarting` and the `restarting` UI string are gone from
      both the Rust side and the page.
- [ ] `cargo build --release`, `cargo clippy --workspace --all-targets` and
      `cargo fmt --check` are clean **on Windows**.
- [ ] The helper and app package specs describe the new behaviour.

## Out of scope

- P0-02 (pipe access control) — a separate task.
- Making the early-inject path safer than "it requires a flag".
