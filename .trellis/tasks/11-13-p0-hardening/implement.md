# Implementation plan — P0 hardening

## Ordered checklist

| # | Task | Gate before the next |
|---|------|----------------------|
| 1 | `11-13-p0-01-no-shellhost-kill` | ShellHost's pid is unchanged across a full app start on Windows |
| 2 | `11-13-p0-03-no-fake-100-percent` | with the helper killed, Quick Settings shows no invented value |
| 3 | `11-13-p0-04-ddc-write-amplification` | a 100-step drag issues a handful of writes and lands the final value |
| 4 | `11-13-p0-02-pipe-acl-and-ratelimit` | a different-SID process cannot connect; a flood cannot exceed the cap |

Each child task carries its own `prd.md`, `design.md` and `implement.md`. This
file is the map; the children are the work.

## Shared validation commands (run on Windows)

```powershell
cargo build --release
cargo clippy --workspace --all-targets
cargo fmt --check
```

`cargo build --release` also builds `brighttray-hook` through `app/build.rs`
and embeds it into `BrightTray.exe`, so a change in any of the three crates is
covered by one build.

Manual smoke test after every task:

```powershell
target\release\brighttray-helper.exe list      # enumeration + brightness read
target\release\brighttray-helper.exe probe     # read-only, never patches
target\release\brighttray-helper.exe set 0 50  # single write
```

## Review gates

- [ ] **Gate A — after task 1.** The app starts, applies a display, and the
      Quick Settings slider appears, *without* the Start menu disappearing at
      any point. Record the ShellHost pid before and after.
- [ ] **Gate B — after task 2.** With the helper killed, the Quick Settings
      tile either does not appear or shows a clearly unknown value — never
      "100 %".
- [ ] **Gate C — after task 3.** The slider still tracks the finger (within the
      throttle interval) and the final value always lands.
- [ ] **Gate D — after task 4.** The hook still connects; a foreign-SID
      process does not; a `setall` flood is capped.

## Cross-child acceptance criteria

See `prd.md` §"Cross-child acceptance criteria". Every box must be ticked
before the parent task is archived.

## Spec updates

Each child updates the package spec it affects, in the same commit as its code:

| Task | Spec files |
|------|------------|
| P0-01 | `spec/brighttray-helper/backend/quality-guidelines.md`, `spec/brighttray-app/backend/quality-guidelines.md`, `spec/brighttray-helper/backend/directory-structure.md` |
| P0-03 | `spec/brighttray-hook/backend/hooking-contract.md` |
| P0-04 | `spec/brighttray-helper/backend/ddc-ci.md`, `spec/brighttray-helper/backend/pipe-protocol.md` |
| P0-02 | `spec/brighttray-helper/backend/pipe-protocol.md`, `spec/brighttray-helper/backend/windows-ffi.md` |

## Out of scope

P1-01 (unit tests, deferred by decision) and everything in P1/P2.

## Rollback points

Each child is one commit and one revert. If a gate fails and the fix is not
immediate, revert that child and continue with the next one — the four
changes are independent.
