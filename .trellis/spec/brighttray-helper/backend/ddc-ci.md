# DDC/CI Access

All DDC/CI traffic goes through `helper/src/ddc.rs`. It is the only file that
calls `GetVCPFeatureAndVCPFeatureReply` / `SetVCPFeature`, and the rules below
exist because monitors drop commands and lie about their brightness.

## The only feature used

`VCP_BRIGHTNESS = 0x10` (luminance). The monitor's own range is read from the
reply (`max`), never assumed to be 100.

## Pacing: 50 ms between two commands to the same monitor

```rust
/// DDC/CI wants roughly 50 ms between two commands to the same monitor. Stricter monitors
/// silently drop a command that follows another one too closely.
const COMMAND_GAP: Duration = Duration::from_millis(50);
```

`Monitor::pace()` sleeps until `COMMAND_GAP` has passed since `Monitor::mark()`.
Both are called inside `Monitor::brightness()` and `Monitor::write_raw()`, so
**calling the dxva2 API directly is forbidden** — go through those two methods
and the pacing comes for free.

A monitor that answers too slowly is not an error, it is just a slow monitor;
the gap is per monitor (`Monitor::last_command`), so two monitors can be driven
in parallel.

## Retries

`RETRIES = 3` with `RETRY_DELAY_MS = 60` between attempts. Both the read and
the write path retry. After `RETRIES` failed attempts the read returns `None`
and the write returns `false` — the caller decides what that means.

## A write is verified, not assumed

`SetVCPFeature` returning success only means the command was *sent*. So
`Monitor::set_brightness_percent()` reads the value back and sends it again up
to `VERIFY_RESENDS = 2` times:

```rust
for _ in 0..=VERIFY_RESENDS {
    if !self.write_raw(value) {
        return false;      // the command could not be sent at all
    }
    match self.brightness() {
        Some(b) if b.current != value => continue,   // did not land, resend
        _ => break,          // landed, or the monitor stopped answering
    }
}
true
```

`false` means "the command could not be sent" (stale handle). A monitor that
keeps ignoring the write still counts as reached — asking again will not
change its mind. Do not turn that `false` into an error for the user.

**The cost of one `set` is 1 write + 1 read in the happy case, and up to
3 writes + 3 reads when the monitor keeps disagreeing.** Every write is an
EEPROM write on the monitor, and a slider drag produces one `set` per step.

## Percentages are scaled to the monitor's range

```rust
// write
let value = (percent.min(100) * max + 50) / 100;
// read
pub fn percent(&self) -> u32 {
    ((self.current * 100 + self.max / 2) / self.max).min(100)
}
```

`percent.min(100)` is applied on the way in, so a caller passing 120 gets 100,
not an overflow. `max` is cached in `Monitor::max` (`Cell<Option<u32>>`) after
the first successful read, so a write does not need a read first.

## Handles die; re-enumerate

`Monitor` owns a `PHYSICAL_MONITOR` handle and releases it in `Drop`
(`DestroyPhysicalMonitor`). Handles go stale after sleep, hot-plug or a remote
session. The recovery lives in the server, in `flush()`:

```rust
if !ok && !refreshed {
    // Handles go stale after sleep or hot-plug: refresh once and retry.
    *monitors = ddc::enumerate();
    refreshed = true;
    if let Some(m) = monitors.get(i) {
        m.set_brightness_percent(p);
    }
}
```

So: re-enumerate **once**, then retry. Never keep a `Vec<Monitor>` across a
re-enumeration, and never assume an index still refers to the same monitor.

## Monitor identity

`Monitor::key()` is `"{device}#{sub_index}|{name}"`, e.g.
`\\.\DISPLAY1#0|Lenovo Y27q-30`. It survives re-enumeration as long as the
display layout does not change.

`server::Target::Monitor { key, name }` falls back to the name when the key is
gone, but **only if exactly one monitor has that name** — ambiguous matches
resolve to nothing rather than to the wrong display.

## "Supports DDC/CI" means "answered a brightness read"

There is no capability flag. `m.brightness().is_some()` is the test, used by
the CLI (`helper/src/main.rs`, `list`), the app (`app/src/main.rs`, `ddc()`) and
the server (`probe()` in `server.rs`). The server caches the answer per monitor
in `capable` and re-probes whenever the monitor list changes, so a slider
poll does not pay the retry delay for monitors that never answer.

## Reading the monitor name

`ddc::enumerate()` prefers the dxva2 description and falls back to the EDID
name, or the other way round — see the comment on `Monitor::name`. The EDID
names come from `friendly_names()`, which maps the GDI device name
(`\\.\DISPLAY1`) to the EDID names of the monitors showing it. The result is a
best-effort human-readable name; "Generic PnP Monitor" is a normal outcome.
