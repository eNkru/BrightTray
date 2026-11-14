# P0-03 — Remove the fake 100 percent brightness fallback

Audit item: **P0-03 去掉 `PIPE_FALLBACK_PERCENT = 100` 的假值**
Source: `/home/hju/sources/pt-monitor-win/artifacts/brighttray-fork-notes.md` §3

## Goal

`hook/src/lib.rs` initialises the cached brightness to 100 %:

```rust
const PIPE_FALLBACK_PERCENT: u32 = 100;
static CACHED_PERCENT: AtomicU32 = AtomicU32::new(PIPE_FALLBACK_PERCENT);
```

That value was never measured. When the helper is not reachable, the hook
tells the shell the brightness is 100 %, the Quick Settings slider appears at
full brightness, and the first drag makes the monitor jump (for example from
20 % to 95 %).

The hook must only ever report a brightness it actually measured.

## Requirements

### R1 — The cached brightness is "known" or "unknown", never a default

The cache starts out **unknown**. There is no fallback number.

### R2 — While the value is unknown, the hook behaves like the un-hooked system

Every property the hook overrides must fall back to the original
implementation while the value is unknown:

- `get_IsApplicable` → the original answer
- `get_IsEnabled` → the original answer
- `GetValue("Value" / "MinValue" / "MaxValue" / "QuickActionStatus" /
  "QuickActionIsActive")` → the original implementation
- `get_QuickActionStatus` → the original implementation
- `get_QuickActionIsActive` → the original answer

On a desktop the original `IsApplicable` is FALSE, so the brightness tile is
simply not shown — an honest absence instead of an invented number. On a
laptop the original reports the panel's own brightness, which is also honest.

### R3 — A measurement makes the value known

- The initial `pipe::get_all()` in `install()` makes the value known.
- The watcher makes it known the first time the helper answers.
- A `SetValue("Value", …)` from the user makes it known — the value came from
  the user's own input.

### R4 — The transition is logged once

When the value becomes known for the first time, the log says so. It must not
say "external brightness change 4294967295% -> 42%".

### R5 — The constant is gone

`PIPE_FALLBACK_PERCENT` is deleted. No default brightness may be introduced
anywhere in the crate.

## Constraints

- The hook's fail-safe rules are unchanged: the vtable slots, the validation
  and the `catch_unwind` wrappers are not touched.
- The pipe protocol is unchanged.
- The helper is unchanged — this task only touches `hook/src/lib.rs`.
- No unit tests in this round.

## Acceptance criteria

Checked here (statically):

- [x] `grep -n "PIPE_FALLBACK_PERCENT" hook/src/lib.rs` returns nothing.
- [x] No default brightness remains in the crate — the cache starts at
      `PERCENT_UNKNOWN` and every override falls back to the original
      implementation while the value is unknown.
- [x] Every override site falls back: `get_IsApplicable`, `get_IsEnabled`,
      `GetValue`, `get_QuickActionStatus`, `get_QuickActionIsActive`, for
      both the `ISettingItem` and the `IQuickAction` hook.
- [x] The hook package spec describes the known/unknown rule.

For the Windows machine (this environment has no Rust toolchain and the
`windows` crate has no non-Windows target, so none of these could be run
here):

- [ ] **Windows check:** start `BrightTray.exe` with the helper killed (or
      kill the helper before the app), open Quick Settings — the brightness
      tile does not appear, and no value of 100 % is reported anywhere.
- [ ] **Windows check:** the other Quick Settings tiles are unaffected in that
      state.
- [ ] **Windows check:** start the helper normally — the tile appears, the
      slider shows the monitor's real brightness, and dragging it works.
- [ ] **Windows check:** the hook log contains `brightness known: N%` the first
      time the helper answers, and `external brightness change X% -> Y%` for
      later changes.
- [ ] **Windows check:** changing the brightness on the monitor's own OSD
      still moves the slider.
- [ ] `cargo build --release`, `cargo clippy --workspace --all-targets` and
      `cargo fmt --check` are clean **on Windows**.

## Out of scope

- A richer "offline" presentation of the tile (a visible tile with a
  placeholder value). The audit allows either "the slider does not appear" or
  "the slider shows it is offline"; this task takes the first, simpler route.
- P0-04 and P0-02.
