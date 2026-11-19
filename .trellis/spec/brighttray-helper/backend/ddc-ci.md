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

`SetVCPFeature` returning success only means the command was *sent*, so a
write has to be followed by a read-back. Writing and verifying are separate
methods, because how much verification is worth paying for depends on the
caller:

| Method | What it does |
|--------|--------------|
| `scale_percent(percent)` | the raw VCP value for `percent`, or `None` when the range is not known yet |
| `write_brightness_percent(percent)` | scale, then `write_raw`; no read-back |
| `verify_brightness(percent)` | read back and compare with the raw value |
| `set_brightness_percent(percent)` | write + verify, up to `VERIFY_RESENDS` resends |

`set_brightness_percent` is what the CLI calls: one deliberate write, so it
reads the value back and sends it again up to `VERIFY_RESENDS = 2` times:

```rust
for _ in 0..=VERIFY_RESENDS {
    if !self.write_brightness_percent(percent) {
        return false;      // the command could not be sent at all
    }
    if self.verify_brightness(percent) {
        break;             // landed, or the monitor stopped answering
    }
}
true
```

`verify_brightness` counts a monitor that does not answer as matched: there is
nothing to compare with, and asking again will not change its mind. A
`scale_percent` that returns `None` means the brightness read did not answer,
so that case is the same "matched" — it must not trigger a resend. Only a
value that was actually read back and differs triggers a resend.

`false` means "the command could not be sent" (stale handle). Do not turn that
`false` into an error for the user.

### The server paces its writes instead of verifying every step

A slider drag produces one `set` per step, and every write is an EEPROM write
on the monitor. The server therefore calls `write_brightness_percent`
unverified during a drag — at most once per `WRITE_INTERVAL` (300 ms), always
with the newest value — and verifies **once**, `VERIFY_DELAY` (500 ms) after
the last write (`server.rs::verify_due`). A mismatch costs exactly one resend,
which is not verified again. A write forced out by a read (`get`, `getall`,
`list`) also schedules that trailing read-back. See
[Pipe Protocol](./pipe-protocol.md) for the cadence.

So the cost is: **one deliberate `set` = 1 write + 1 read** (up to 3 + 3 when
the monitor keeps disagreeing), **a forty-step drag ≈ 7 writes + 1 read**.

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
`Monitor::scale_percent()` is exactly this computation and is public because
the server compares a read-back with the raw value it wrote, not with the
percentage.

## Handles die; re-enumerate

`Monitor` owns a `PHYSICAL_MONITOR` handle and releases it in `Drop`
(`DestroyPhysicalMonitor`). Handles go stale after sleep, hot-plug or a remote
session. The recovery lives in the server's write functions, `write_values()`
(and the resend in `verify_due()`), and every re-enumeration goes through
`server.rs::reenumerate()`:

```rust
if !ok && !refreshed {
    // Handles go stale after sleep or hot-plug: refresh once and retry.
    reenumerate(monitors, pending, throttle, verify);
    refreshed = true;
    if let Some(i) = index_of(monitors, &key) {
        monitors[i].write_brightness_percent(p);
    }
}
```

`reenumerate` re-keys the cadence state (`pending`, `throttle`, `verify`) by
`Monitor::key()` instead of by index, because an index only means something
for one enumeration. State for a monitor that disappeared is dropped: there is
nothing left to write to or read back from, and it is not re-enumerated again.
`write_values()` resolves its whole batch to `Monitor::key()` **before** the
first write for the same reason, so the values that are still to come cannot
land on a different monitor after a refresh in the middle of the batch.

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
