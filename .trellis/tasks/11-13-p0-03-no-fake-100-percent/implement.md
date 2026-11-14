# Implementation plan — P0-03

All changes are in `hook/src/lib.rs`.

## Step 1 — The cache becomes "known or unknown"

- [x] Replace `const PIPE_FALLBACK_PERCENT: u32 = 100;` with
      `const PERCENT_UNKNOWN: u32 = u32::MAX;` and document why the sentinel
      cannot collide with a percent.
- [x] Initialise `CACHED_PERCENT` with `PERCENT_UNKNOWN`.
- [x] Add `fn cached_percent() -> Option<u32>`.
- [x] Add `original_is_enabled` and `original_is_applicable` to `State`, and
      save them in `try_install()` next to the other originals. Both slots (9
      and 10) are already in the validated slot list, so their addresses are
      checked before the vtable is patched.

## Step 2 — Every override falls back while the value is unknown

- [x] `get_value_impl`: if `cached_percent()` is `None`, call
      `(state.original_get_value)(this, name, out)` and return.
- [x] `hooked_is_enabled` / `hooked_is_applicable`: if unknown, call the
      original and return its `HRESULT`.
- [x] `quick_get_value_impl`: same fallback via `state.original_get_value`.
- [x] `quick_is_applicable` / `quick_is_enabled` / `quick_is_active` /
      `quick_status`: same fallback via the corresponding original.
- [x] `set_value_impl` / `quick_set_value_impl`: unchanged — the value comes
      from the user and is therefore known after the store.

## Step 3 — The transition log

- [x] `publish_percent(percent)` owns the swap and the two-arm match on
      `PERCENT_UNKNOWN`: `brightness known: N%` for the first measurement,
      `external brightness change X% -> Y%` for every later one.
- [x] `install()` publishes through it, so the *first* helper answer is
      logged too (the watcher would otherwise find the value unchanged and
      log nothing at all).
- [x] `store_user_percent(percent)` is the `SetValue` path: the value becomes
      known there, and only that first transition is logged — a later move is
      the user's, not an external change.

## Step 4 — The crate's own docs

- [x] The module doc block at the top of `lib.rs` mentions what the hook
      reports — update it if it describes the old fallback.
- [x] Check the `PIPE_FALLBACK_PERCENT` mention in any comment.

## Step 5 — Spec

- [x] `spec/brighttray-hook/backend/hooking-contract.md` — the "The value the
      shell sees" section describes the known/unknown rule instead of the
      fallback constant.
- [x] `spec/brighttray-hook/backend/logging-guidelines.md` — the "first
      measurement" line.
- [x] `spec/brighttray-hook/backend/pipe-client.md` — `get_all()` has two
      callers, not one.
- [x] `spec/brighttray-helper/backend/pipe-protocol.md` — the "helper is not
      running" case.

## Step 6 — Review findings (check agent)

Fixed in this task:

- [x] **The "brightness known" line never appeared.** `install()` stored the
      first measurement without logging it, and the watcher then found the
      value unchanged — so the log stayed silent in the normal flow. Both
      paths now publish through `publish_percent`.
- [x] **The unknown value could become permanent.** The watcher only polled
      the helper when a UI handler was registered, but an unknown value keeps
      the tile hidden and a hidden tile registers no handler. The watcher now
      keeps polling until a value has been measured.
- [x] The module doc still claimed `get_IsEnabled` / `get_IsApplicable`
      "report TRUE" unconditionally.

Left alone on purpose (pre-existing, out of scope):

- [ ] `quick_status` writes an `IPropertyValue` into the `HSTRING*` out
      parameter of `get_QuickActionStatus` — a type confusion that predates
      this task. It needs its own task and a Windows check.
- [ ] `hooked_is_enabled`, `hooked_is_applicable`, `quick_is_applicable`,
      `quick_is_enabled`, `quick_is_active` and `quick_status` are patched
      into the vtable without a `catch_unwind` wrapper. Also pre-existing.

## Step 7 — Verification (on Windows)

**Not run here — this environment has no Rust toolchain and the `windows`
crate does not build for non-Windows targets.** Every box below is for the
Windows machine.

- [ ] `cargo build --release`
- [ ] `cargo clippy --workspace --all-targets`
- [ ] `cargo fmt --check`
- [ ] `grep -n "PIPE_FALLBACK_PERCENT\|100" hook/src/lib.rs` — no fallback
      constant, and no other invented 100 % value.
- [ ] **Helper down:** kill `brighttray-helper.exe serve`, then open Quick
      Settings — no brightness tile on a desktop, no 100 % anywhere; the other
      tiles are unaffected.
- [ ] **Helper up:** start the helper, open Quick Settings — the tile appears
      with the real value; drag it, the monitor follows.
- [ ] The hook log shows `brightness known: N%` once, then
      `external brightness change X% -> Y%` for later changes.
- [ ] Change the brightness in the monitor's OSD — the slider follows back.
- [ ] **Helper up late:** start the hook with the helper down, then start the
      helper — the tile appears without a shell restart.

## Rollback

One commit, one revert: restore `PIPE_FALLBACK_PERCENT` and the unconditional
reads. The hook's vtable slots and validation are untouched, so a revert
cannot leave the shell patched differently.
