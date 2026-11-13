# Design — P0-03

## The problem in one line

`CACHED_PERCENT` is an `AtomicU32` whose initial value is a constant that was
never measured, and every read path uses it unconditionally.

## Decision 1 — One atomic, one sentinel

The state is "a percent I measured" or "I have not measured one". A separate
validity flag would mean two atomics that must be ordered against each other
(a reader could see "valid" before the value). A sentinel in the same atomic
avoids that entirely:

```rust
/// The cached brightness in percent, or `PERCENT_UNKNOWN` when it has never
/// been measured. A percent is 0..=100, so `u32::MAX` cannot collide.
const PERCENT_UNKNOWN: u32 = u32::MAX;
static CACHED_PERCENT: AtomicU32 = AtomicU32::new(PERCENT_UNKNOWN);

/// The cached brightness, or `None` when it was never measured.
fn cached_percent() -> Option<u32> {
    match CACHED_PERCENT.load(Ordering::Relaxed) {
        PERCENT_UNKNOWN => None,
        percent => Some(percent),
    }
}
```

`PERCENT_UNKNOWN` replaces `PIPE_FALLBACK_PERCENT`. The name says what it is;
the old name said what it was for.

## Decision 2 — Unknown means "call the original"

Every override site gets the same shape:

```rust
let Some(percent) = cached_percent() else {
    // We have never measured the brightness. Behave exactly like the
    // un-hooked system instead of inventing a value.
    return (state.original_get_value)(this, name, out);
};
```

For the boolean getters:

```rust
if cached_percent().is_none() {
    return (state.original_is_applicable)(this, out);
}
```

The `IQuickAction` hook already keeps `original_is_applicable`,
`original_is_enabled`, `original_status` and `original_is_active` in
`QuickState`, so no new state is needed there.

The `ISettingItem` hook does **not** keep the two originals — its
`hooked_is_enabled` / `hooked_is_applicable` always answer 1. Two originals
are added to `State`:

```rust
struct State {
    original_is_enabled: GetBoolFn,
    original_is_applicable: GetBoolFn,
    original_get_value: GetValueFn,
    …
}
```

Both slots (9 and 10) are already in the validated slot list of
`try_install()`, and their addresses are checked against the module image
before the vtable is patched, so saving them is safe.

## What the user sees

| Situation | Before | After |
|-----------|--------|-------|
| Helper up, everything normal | tile with the real value | unchanged |
| Helper killed before the hook installs | tile showing **100 %**, jumps on the first drag | **no tile** (a desktop) or the system's own value (a laptop) |
| Helper killed after the value was known | tile with the last known value | unchanged — the last measured value stays |
| Helper comes back | value updates on the next poll | unchanged, plus one `brightness known: N%` line |

The third row matters: once a real value has been measured, keeping it is
honest. The audit's complaint is about the *invented* value, not about caching.

## The transition log

```rust
if let Some(percent) = pipe::get_all() {
    let previous = CACHED_PERCENT.swap(percent, Ordering::Relaxed);
    if previous != percent {
        match previous {
            PERCENT_UNKNOWN => log(format!("brightness known: {percent}%")),
            previous => log(format!("external brightness change {previous}% -> {percent}%")),
        }
        notify_handlers();
    }
}
```

The first line reads differently from the later ones, because the first
reading is not a *change* — there was no previous value. `notify_handlers()`
still runs, so an open slider refreshes to the now-known value.

## Interaction with the other P0 tasks

- **P0-01** is independent. The hook does not know about the shell restart.
- **P0-04** changes when the helper writes, not what the hook caches. When a
  write is deferred by the throttle, the hook's cached value lags by up to the
  throttle interval — the existing 1.2 s "do not fight the finger" window
  already covers that.
- **P0-02** is independent.

## Interaction with the shell's own caching

Quick Settings caches which tiles are applicable when it builds its panel. If
the tile was hidden because the value was unknown, it comes back when the
panel is rebuilt and the value is known. The app starts the helper **before**
injecting the hook, so in the normal flow the value is known at install time
and the tile appears on the first panel build.

## Risks

| Risk | Mitigation |
|------|------------|
| The tile does not appear when the helper starts late | The app starts the helper first; the CLI's `serve` serves the pipe before injecting. The tray action from P0-01 covers the rest. |
| `ISettingItem`'s original answers differ from what the hook used to claim | That is the point — the hook now reports the system's own state instead of a fabricated one. The `ISettingItem` hook is a compatibility layer for the Settings page, not for Quick Settings (per `docs/quick-settings-hook-research.md` §7). |
| A stale `PERCENT_UNKNOWN` leaks into a comparison | Only two comparison sites exist (the watcher's `swap` and the getters); both go through `cached_percent()`. |
