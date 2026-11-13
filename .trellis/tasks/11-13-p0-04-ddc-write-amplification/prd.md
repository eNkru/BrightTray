# P0-04 — Reduce DDC write amplification during slider drags

Audit item: **P0-04 降低 DDC 写入放大**
Source: `/home/hju/sources/pt-monitor-win/artifacts/brighttray-fork-notes.md` §3

## Goal

One brightness change currently costs one DDC/CI **write** plus one
**read-back**, repeated up to three times:

```
helper/src/ddc.rs — set_brightness_percent()
    write_raw(value)        ← EEPROM write
    brightness()            ← read-back
    if the value differs → write_raw + brightness() again   (VERIFY_RESENDS = 2)
```

A slider drag produces one `set` per step. The audit's measurement of a
100-step drag: up to 100 writes and 100 reads, i.e. up to 300 monitor writes.
Every write is an EEPROM write on the monitor.

The write rate must be bounded, and the read-back must happen once at the end
of a drag instead of on every step.

## Requirements

### R1 — A drag writes at most once per throttle interval

Two consecutive writes to the same monitor must be at least
`WRITE_INTERVAL` apart. The value that is written is always the **latest**
one requested — a throttled write is never stale.

### R2 — The first write of a drag is immediate

The throttle must not delay the first value. A user who moves the slider and
sees the monitor react right away is the baseline; only the *rate* changes.

### R3 — The final value always lands

After the last write, the value that was requested last must have been
written. A trailing write is part of the design, not an accident of the
timing.

### R4 — The read-back happens once per drag, at the end

During a drag, writes are not verified. One read-back runs after the writes
have settled. If the monitor did not take the value, it is sent **once** more
— and that resend is not verified again.

### R5 — The helper keeps its guarantees

- The 50 ms DDC/CI command gap still holds between any two commands to the
  same monitor.
- A stale monitor handle still triggers exactly one re-enumeration and a
  retry.
- A write that could not be sent at all still reports failure.

### R6 — The CLI's single `set` still verifies

`brighttray-helper set <index> <percent>` is one deliberate write. It keeps
the write-then-verify behaviour and its current exit codes.

## Constraints

- The change is inside the helper: `helper/src/ddc.rs` and
  `helper/src/server.rs`. The hook is not involved — it sends `setall` and
  does not know about the timing.
- The pipe protocol does not change: `set` and `setall` still have no reply.
- A read (`get`, `getall`, `list`) must reflect the value that was last
  requested, even if the write is still throttled. The read path therefore
  forces the pending write out before reading.
- No unit tests in this round.

## Acceptance criteria

- [ ] Two writes to the same monitor are never closer than
      `WRITE_INTERVAL` (300 ms) — unless a read forces the pending value out.
- [ ] The first write after a quiet period happens immediately.
- [ ] The last value of a drag is written within `WRITE_INTERVAL` of the drag
      ending.
- [ ] A drag produces one read-back, not one per step.
- [ ] A monitor that ignores the write gets exactly one resend.
- [ ] **Windows check:** drive 100 `setall` commands in a burst and count the
      `SetVCPFeature` calls — the count is in the low single digits, not 100+.
- [ ] **Windows check:** drag the Quick Settings slider by hand — the monitor
      follows, with a visible lag of at most about a third of a second, and
      the final value lands.
- [ ] **Windows check:** `brighttray-helper set 0 50` still reports success
      and the monitor is at 50 %.
- [ ] **Windows check:** the "external brightness change" feedback still works
      (the OSD → slider direction).
- [ ] `cargo build --release`, `cargo clippy --workspace --all-targets` and
      `cargo fmt --check` are clean **on Windows**.
- [ ] The helper package spec describes the new write cadence.

## Out of scope

- P0-02's token bucket — a separate task that refines this one.
- Making the write cadence configurable from the UI.
