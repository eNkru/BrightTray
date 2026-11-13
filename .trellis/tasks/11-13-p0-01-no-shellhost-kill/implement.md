# Implementation plan — P0-01

## Step 1 — `helper/src/inject.rs`

- [x] Remove the `restart_stale: bool` parameter from `watch_and_inject` and
      delete the stale-restart branch.
- [x] Delete `FRESH_SHELLHOST`, `RESTART_COOLDOWN`, `process_age()` and the
      `last_restart` local.
- [x] Keep the per-pid terminate helper private — renamed to
      `terminate_shellhost(pid)` because Rust has no overloading — and add the
      public `restart_shellhost() -> Result<(), String>` that ends every
      ShellHost pid.
- [x] Update the doc comment of `inject_early()` to state that it terminates
      every running ShellHost and that callers must have the user's consent.
- [x] Update the module doc block at the top of the file — it still describes
      the restart behaviour.
- [x] Identify a process by its pid **and** its creation time, so a restarted
      ShellHost that reuses the pid is not mistaken for the hooked one.
- [x] Do not report an injection failure for a process that is already
      exiting (`process_gone`).

## Step 2 — `helper/src/main.rs`

- [x] `inject --early` requires `--force-restart`; otherwise print the
      consequence and the hint, and return the usage exit code (2).
- [x] Update `usage()` so the new flag combination is documented.
- [x] The `serve` command calls `watch_and_inject(dll, report)` without the
      removed parameter.

## Step 3 — `app/src/main.rs`

- [x] Call `watch_and_inject(dll, report)` without the removed parameter.
- [x] Add the tray menu item with the bilingual label that states the
      consequence.
- [x] Handle the menu click: refuse with a bilingual notice when the watcher
      is not running yet, otherwise run `inject::restart_shellhost()` off the
      event loop thread and report the result through a new `UserEvent`.
- [x] Bring the window forward when there is a notice to show — the app lives
      in the tray, so a notice that only lives in the page is invisible.
- [x] Set `watcher_started` only after `hookdll::extract()` succeeded, so the
      tray item is not offered when no watcher thread exists.
- [x] Remove the `WatchEvent::Restarting` arm from `push_state()`.
- [x] Add the two new bilingual strings through `tr()`.

## Step 4 — `app/ui/index.html`

- [x] Remove the `restarting` arm from `statusOf()`.
- [x] Remove `restarting` from both `STRINGS.zh` and `STRINGS.en`.

## Step 5 — `README.md`

- [x] Update the CLI table: `inject [--early --force-restart] [dll]`.
- [x] Add a line to the FAQ explaining that BrightTray never restarts
      ShellHost by itself, and how to force it (tray menu item, or the CLI
      flag).
- [x] The FAQ answer for "the slider doesn't appear" should mention the tray
      item as the recovery path.

## Step 6 — Spec

- [x] `spec/brighttray-helper/backend/quality-guidelines.md` — the "The
      helper terminates ShellHost on purpose" section becomes "the helper
      never terminates ShellHost except through the explicit paths".
- [x] `spec/brighttray-helper/backend/directory-structure.md` — the `inject`
      row and the hard-coded paths note.
- [x] `spec/brighttray-app/backend/quality-guidelines.md` — the "The app
      restarts ShellHost" section becomes "the app never restarts ShellHost
      automatically; the tray menu can do it on request".
- [x] `spec/brighttray-app/backend/ui-contract.md` and `i18n.md` — the
      `Restarting` row and the `restarting` status key are gone.

## Step 7 — Verification (on Windows)

**Not run here — this environment has no Rust toolchain and the `windows`
crate does not build for non-Windows targets.** Every box below is for the
Windows machine.

- [ ] `cargo build --release`
- [ ] `cargo clippy --workspace --all-targets`
- [ ] `cargo fmt --check`
- [ ] `target\release\brighttray-helper.exe list` — still enumerates.
- [ ] `target\release\brighttray-helper.exe inject --early` — prints the
      explanation, terminates nothing, exit code 2.
- [ ] `target\release\brighttray-helper.exe inject --early --force-restart` —
      still injects.
- [ ] Start `BrightTray.exe`, record the ShellHost pid, press **Apply**, and
      confirm the pid never changes.
- [ ] Confirm the Quick Settings slider appears (cold start and a ShellHost
      that has been alive for an hour).
- [ ] Click the tray item: ShellHost restarts, the status line goes to
      "等待 ShellHost…" and back to "运行中", and the slider still appears.
- [ ] Confirm the other Quick Settings tiles are unaffected.

## Follow-ups found during the review

- [x] **PID reuse.** The watcher's `hooked` map now remembers the pid *and*
      the process creation time, so a restarted ShellHost that reuses the pid
      is recognised as a different process and gets the hook.
- [ ] The `waiting` state ("等待 ShellHost…") is best-effort: `sihost` usually
      restarts ShellHost faster than the watcher notices, so the status line
      may stay on "运行中" throughout the restart.

## Rollback

One commit, one revert: restore the `restart_stale` parameter and the branch,
drop the tray item, and put the `--force-restart` guard back out.
