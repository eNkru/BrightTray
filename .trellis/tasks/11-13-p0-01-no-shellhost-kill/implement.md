# Implementation plan — P0-01

## Step 1 — `helper/src/inject.rs`

- [ ] Remove the `restart_stale: bool` parameter from `watch_and_inject` and
      delete the stale-restart branch.
- [ ] Delete `FRESH_SHELLHOST`, `RESTART_COOLDOWN`, `process_age()` and the
      `last_restart` local.
- [ ] Keep `restart_shellhost(pid)` as a private helper; add the public
      `restart_shellhost() -> Result<(), String>` that terminates every
      ShellHost pid and reports how many were ended.
- [ ] Update the doc comment of `inject_early()` to state that it terminates
      every running ShellHost and that callers must have the user's consent.
- [ ] Update the module doc block at the top of the file — it still describes
      the restart behaviour.

## Step 2 — `helper/src/main.rs`

- [ ] `inject --early` requires `--force-restart`; otherwise print the
      consequence and the hint, and return the usage exit code (2).
- [ ] Update `usage()` so the new flag combination is documented.
- [ ] The `serve` command calls `watch_and_inject(dll, report)` without the
      removed parameter.

## Step 3 — `app/src/main.rs`

- [ ] Call `watch_and_inject(dll, report)` without the removed parameter.
- [ ] Add the tray menu item with the bilingual label that states the
      consequence.
- [ ] Handle the menu click: refuse with a bilingual notice when the watcher
      is not running yet, otherwise run `inject::restart_shellhost()` off the
      event loop thread and report the result through a new `UserEvent`.
- [ ] Remove the `WatchEvent::Restarting` arm from `push_state()`.
- [ ] Add the two new bilingual strings through `tr()`.

## Step 4 — `app/ui/index.html`

- [ ] Remove the `restarting` arm from `statusOf()`.
- [ ] Remove `restarting` from both `STRINGS.zh` and `STRINGS.en`.

## Step 5 — `README.md`

- [ ] Update the CLI table: `inject [--early --force-restart] [dll]`.
- [ ] Add a line to the FAQ explaining that BrightTray never restarts
      ShellHost by itself, and how to force it (tray menu item, or the CLI
      flag).
- [ ] The FAQ answer for "the slider doesn't appear" should mention the tray
      item as the recovery path.

## Step 6 — Spec

- [ ] `spec/brighttray-helper/backend/quality-guidelines.md` — the "The
      helper terminates ShellHost on purpose" section becomes "the helper
      never terminates ShellHost except through the explicit paths".
- [ ] `spec/brighttray-helper/backend/directory-structure.md` — the `inject`
      row and the hard-coded paths note.
- [ ] `spec/brighttray-app/backend/quality-guidelines.md` — the "The app
      restarts ShellHost" section becomes "the app never restarts ShellHost
      automatically; the tray menu can do it on request".

## Step 7 — Verification (on Windows)

```powershell
cargo build --release
cargo clippy --workspace --all-targets
cargo fmt --check
```

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

## Rollback

One commit, one revert: restore the `restart_stale` parameter and the branch,
drop the tray item, and put the `--force-restart` guard back out.
