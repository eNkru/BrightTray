# Design — P0-04

## Current write path

`helper/src/server.rs`:

```
worker loop
  └─ rx.recv()  → batch → Cmd::SetAll(p) → pending.insert(i, p)
  └─ flush(&mut monitors, &mut pending)
       └─ for each (i, p): monitors[i].set_brightness_percent(p)
            └─ write_raw(value)          ← EEPROM write
            └─ brightness()              ← read-back
            └─ if it differs → repeat (up to VERIFY_RESENDS = 2)
```

The coalescing that already exists (`pending: BTreeMap<usize, u32>`) only
helps *within* one batch. A drag delivers its steps spread over time, and
each batch ends in a full write+verify cycle.

## Decision 1 — Split "write" from "verify" in `ddc.rs`

The verification is a property of *how sure we are*, not of *how we write*.
Splitting it lets the server choose the policy:

```rust
impl Monitor {
    /// The raw VCP value for `percent` on this monitor, or `None` when the
    /// monitor's range is not known yet.
    pub fn scale_percent(&self, percent: u32) -> Option<u32>;

    /// Sends one brightness write without reading it back.
    pub fn write_brightness_percent(&self, percent: u32) -> bool;

    /// Reads the brightness back and compares it with `percent`.
    pub fn verify_brightness(&self, percent: u32) -> bool;

    /// Writes and verifies, retrying at most `VERIFY_RESENDS` times.
    /// This is the CLI's single-shot path.
    pub fn set_brightness_percent(&self, percent: u32) -> bool;
}
```

`set_brightness_percent` keeps its current behaviour exactly — it becomes the
composition of the two new methods plus the retry loop. The CLI keeps using
it.

`scale_percent` is needed by the server because the verification compares the
**raw** VCP value, not the percent: the monitor's own `max` is the scale, and
the read-back returns raw values.

## Decision 2 — The worker owns the write cadence

The server's worker thread is the only place that writes to a monitor, so it
is the only place that can pace the writes. It grows three pieces of state:

```rust
/// The value that still has to be written, per monitor.
pending: BTreeMap<usize, u32>,
/// When the last write to each monitor happened.
throttle: BTreeMap<usize, Instant>,
/// The trailing verification: the value that was written, and when it is due.
verify: BTreeMap<usize, (u32, Instant)>,
```

Constants:

```rust
/// Minimum interval between two writes to the same monitor.
const WRITE_INTERVAL: Duration = Duration::from_millis(300);
/// How long after the last write the read-back runs.
const VERIFY_DELAY: Duration = Duration::from_millis(500);
```

## Decision 3 — The worker loop gets a deadline

The current loop blocks in `rx.recv()`. It now blocks in
`rx.recv_timeout(deadline)`, where the deadline is the earliest of:

- the moment a pending value may be written (its throttle window ends),
- the moment a trailing verification is due.

```rust
loop {
    match next_deadline(&pending, &throttle, &verify) {
        Some(deadline) => match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(cmd) => handle(cmd),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        },
        None => match rx.recv() {
            Ok(cmd) => handle(cmd),
            Err(_) => break,
        },
    }
    if write_due(&mut monitors, &mut pending, &mut throttle, &mut verify) {
        capable = None;
    }
    verify_due(&mut monitors, &mut verify);
}
```

`write_due()`:

- For every monitor with a pending value, if the throttle window has passed
  (or the monitor was never written to), write the **latest** value, record
  the write time, and schedule the trailing verification.
- If the throttle window has not passed, the value stays pending. It is
  written at the next opportunity — a throttled write is never stale,
  because `pending` holds only the newest value.

`verify_due()`:

- For every monitor whose verification is due, read the brightness back.
- If it differs from the value that was written, write it **once** more.
  That resend is not verified again.

## Decision 4 — Reads force the pending write out

`Cmd::Get`, `Cmd::GetAll` and `Cmd::List` answer "what is the brightness
now". If a write is still throttled, the honest answer would be the old
value — but the user asked for a new one and the shell is about to be told
about it. The read path therefore writes the pending value first, ignoring the
throttle window.

This cannot turn into a write amplifier:

- The hook's watcher skips its poll for 1.2 s after a UI-originated write,
  so it does not poll during a drag.
- A read only writes when something is actually pending, which only a `set`
  can create.

## The resulting timeline

A burst of 100 `setall` commands within 100 ms, on one monitor:

```
t=0      the first set writes immediately          → 1 write
t=0..100 the remaining 99 sets replace `pending`   → 0 writes
t=300    the throttle window ends, the last value   → 1 write
         is written
t=800    the read-back runs                         → 1 read
         (a mismatch triggers 1 more write)
```

Two writes and one read, instead of up to 100 writes and 100 reads.

A hand drag over two seconds, one step every 50 ms:

```
t=0, 300, 600, 900, …  one write per 300 ms, each carrying the newest value
t=2100                 the final value is written
t=2600                 one read-back
```

The monitor follows the finger with a lag of at most 300 ms, and the drag
costs about seven writes instead of forty.

## Decision 5 — The CLI keeps the verified path

`brighttray-helper set <index> <percent>` calls `set_brightness_percent`,
which still writes and verifies (and retries once). A single deliberate write
should confirm that it landed, and the CLI is the only place where the user
waits for the answer.

## Interaction with the other P0 tasks

- **P0-01** is independent.
- **P0-03** is independent — the hook caches what it was told, not what the
  helper wrote. When a write is deferred by the throttle, the hook's cached
  value lags by up to `WRITE_INTERVAL`, which the hook's existing 1.2 s
  "do not fight the finger" window already covers.
- **P0-02** adds a global token bucket to the same worker loop. It is
  implemented afterwards, on top of this cadence.

## Risks

| Risk | Mitigation |
|------|------------|
| The slider feels laggy | The lag is bounded by `WRITE_INTERVAL` and only exists while the value is changing. The first write is immediate and the final value always lands. |
| A value is never written because the throttle window never opens | `write_due` is called on every loop iteration, including the timeout branch; the window always opens within `WRITE_INTERVAL`. |
| The verification fights the user's next drag | The resend only happens if the read-back differs, and it writes the same value the user asked for. |
| The stale-handle refresh is lost | The write path keeps the existing "re-enumerate once and retry" behaviour; `write_due` returns whether the monitor list was refreshed, exactly like `flush` does today. |
