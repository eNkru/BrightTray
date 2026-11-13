# Implementation plan — P0-03

All changes are in `hook/src/lib.rs`.

## Step 1 — The cache becomes "known or unknown"

- [ ] Replace `const PIPE_FALLBACK_PERCENT: u32 = 100;` with
      `const PERCENT_UNKNOWN: u32 = u32::MAX;` and document why the sentinel
      cannot collide with a percent.
- [ ] Initialise `CACHED_PERCENT` with `PERCENT_UNKNOWN`.
- [ ] Add `fn cached_percent() -> Option<u32>`.
- [ ] Add `original_is_enabled` and `original_is_applicable` to `State`, and
      save them in `try_install()` next to the other originals.

## Step 2 — Every override falls back while the value is unknown

- [ ] `get_value_impl`: if `cached_percent()` is `None`, call
      `(state.original_get_value)(this, name, out)` and return.
- [ ] `hooked_is_enabled` / `hooked_is_applicable`: if unknown, call the
      original and return its `HRESULT`.
- [ ] `quick_get_value_impl`: same fallback via `state.original_get_value`.
- [ ] `quick_is_applicable` / `quick_is_enabled` / `quick_is_active` /
      `quick_status`: same fallback via the corresponding original.
- [ ] `set_value_impl` / `quick_set_value_impl`: unchanged — the value comes
      from the user and is therefore known after the store.

## Step 3 — The transition log

- [ ] In `start_watcher`, replace the single `external brightness change`
      line with the two-arm match on `PERCENT_UNKNOWN`.

## Step 4 — The crate's own docs

- [ ] The module doc block at the top of `lib.rs` mentions what the hook
      reports — update it if it describes the old fallback.
- [ ] Check the `PIPE_FALLBACK_PERCENT` mention in any comment.

## Step 5 — Spec

- [ ] `spec/brighttray-hook/backend/hooking-contract.md` — the "The value the
      shell sees" section describes the known/unknown rule instead of the
      fallback constant.

## Step 6 — Verification (on Windows)

```powershell
cargo build --release
cargo clippy --workspace --all-targets
cargo fmt --check
```

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

## Rollback

One commit, one revert: restore `PIPE_FALLBACK_PERCENT` and the unconditional
reads. The hook's vtable slots and validation are untouched, so a revert
cannot leave the shell patched differently.
