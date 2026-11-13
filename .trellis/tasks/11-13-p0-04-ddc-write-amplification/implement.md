# Implementation plan — P0-04

## Step 1 — `helper/src/ddc.rs`

- [ ] Add `pub fn scale_percent(&self, percent: u32) -> Option<u32>` — the
      scaled raw value, or `None` when the monitor's `max` is unknown.
- [ ] Add `pub fn write_brightness_percent(&self, percent: u32) -> bool` —
      scale, then `write_raw`; no read-back.
- [ ] Add `pub fn verify_brightness(&self, percent: u32) -> bool` — scale, then
      `brightness()` and compare the raw value.
- [ ] Rewrite `set_brightness_percent` as the composition of the two, keeping
      its current retry count and its current return semantics.
- [ ] Keep the `COMMAND_GAP` pacing inside `write_raw` / `brightness`
      unchanged.

## Step 2 — `helper/src/server.rs` — the worker state

- [ ] Add the constants `WRITE_INTERVAL = 300 ms` and `VERIFY_DELAY = 500 ms`.
- [ ] Replace `pending: BTreeMap<usize, u32>` with the three maps: `pending`,
      `throttle`, `verify`.
- [ ] Add `fn next_deadline(...) -> Option<Instant>` — the earliest moment a
      pending write or a verification is due.
- [ ] Add `fn write_due(monitors, pending, throttle, verify) -> bool` —
      writes the values whose throttle window has passed, schedules the
      trailing verification, and returns whether the monitor list was
      re-enumerated.
- [ ] Add `fn verify_due(monitors, verify) -> bool` — reads back the due
      verifications and resends once on a mismatch.

## Step 3 — `helper/src/server.rs` — the loop

- [ ] Change the worker loop from `rx.recv()` to a deadline-aware loop with
      `rx.recv_timeout(...)`, and `rx.recv()` when there is no deadline.
- [ ] Keep the batch drain (`rx.try_iter()`) so a burst becomes one pass.
- [ ] Call `write_due` and `verify_due` after every iteration, including the
      timeout branch.
- [ ] `Cmd::Get` / `Cmd::GetAll` / `Cmd::List` force the pending writes out
      before reading (ignore the throttle window for that call).
- [ ] Delete `flush()` once nothing calls it any more, or keep it as the
      "force" variant — whichever reads better.

## Step 4 — `helper/src/server.rs` — the module doc

- [ ] The module header documents the reply formats; add the write cadence
      to it, because it is now part of the server's contract.

## Step 5 — Spec

- [ ] `spec/brighttray-helper/backend/ddc-ci.md` — the write path is now
      write-only during a drag plus one trailing read-back; document the
      throttle, the trailing verification and the single resend.
- [ ] `spec/brighttray-helper/backend/pipe-protocol.md` — a read forces the
      pending write out; document it in the command table.

## Step 6 — Verification (on Windows)

```powershell
cargo build --release
cargo clippy --workspace --all-targets
cargo fmt --check
```

- [ ] `target\release\brighttray-helper.exe set 0 50` — still verifies and
      still reports success.
- [ ] **Write counting:** drive 100 `setall` commands in a burst (a small
      script over the pipe) and count the `SetVCPFeature` calls — low single
      digits, not 100+.
- [ ] **Hand drag:** open Quick Settings, drag the brightness slider slowly —
      the monitor follows with a small lag and the final value lands exactly.
- [ ] **Hand drag, fast:** drag quickly — the final value still lands.
- [ ] The hook log shows the expected `external brightness change` lines, not
      a flood.
- [ ] `brighttray-helper watch` still prints the slider changes.

## Rollback

One commit, one revert: restore `flush()` and the verified
`set_brightness_percent` call in the write path. The pipe protocol and the
hook are untouched, so a revert cannot leave the two ends disagreeing.
