# P0 hardening — BrightTray

## Goal

Close the four P0 findings of the BrightTray audit so that the app stops
terminating the user's shell, stops reporting a brightness it never measured,
stops amplifying DDC/CI writes during a slider drag, and stops letting any
same-user process drive the monitor through the named pipe.

The audit lives outside this repository, in the sibling checkout:
`/home/hju/sources/pt-monitor-win/artifacts/brighttray-fork-notes.md`. It was
written against commit `26e73eb`; the product source has not changed since, so
every finding still applies.

## Requirements

### R1 — No automatic termination of ShellHost

Nothing in the normal flow may end a `ShellHost.exe` process. The Start menu
and Quick Settings disappear while the shell is down, which the user perceives
as a crash of Windows itself.

The capability to force a restart must survive as an **explicit, user-visible
action** — never as a default.

### R2 — Never report a brightness that was not measured

When the helper is unreachable, the hook must not tell the shell "100 %".
Either the value is known, or the hook behaves exactly like the un-hooked
system.

### R3 — One slider drag must not become dozens of monitor writes

A drag currently produces one verified write per step: one DDC/CI write plus
one read-back, repeated up to three times. The write rate must be bounded, and
the read-back must happen once at the end rather than on every step.

### R4 — The pipe must not be an open DDC/CI channel for every local process

`\\.\pipe\BrightTray` is created with the default security descriptor, so any
process running as the same user can connect and issue writes without any
rate limit. The pipe must be restricted to the current user, and the number of
monitor writes per second must be capped.

### R5 — No behaviour regression in what already works

- The pipe protocol (`list` / `get` / `set` / `getall` / `setall`) keeps its
  wire format and its reply semantics. The hook in `ShellHost.exe` must keep
  working against the helper without a coordinated upgrade.
- The app keeps its current UX: apply a display, see the slider in Quick
  Settings, drag it, and see the monitor follow.
- The CLI keeps working: `list`, `set`, `serve`, `inject`, `probe`, `watch`.

### R6 — No unit tests in this round

The audit's P1-01 asks for a unit-test suite. The developer has decided to
defer it. This round is **behaviour change only**; the verification is manual,
on Windows.

## Constraints

| Constraint | Consequence |
|------------|-------------|
| The workspace builds for Windows only (`windows` crate 0.58, no `cfg` fallbacks) | `cargo build` / `clippy` / `fmt` **cannot run in the development environment used to write this code** — there is no Rust toolchain there, and the `windows` crate has no non-Windows target. Every task must be verified on a Windows machine. |
| The hook runs inside `ShellHost.exe` | A fault in the hook takes the user's shell down. The fail-safe rules in the hook spec stay in force. |
| The helper is the only DDC/CI speaker | The rate limit and the write throttle both belong in the helper, never in the hook. |
| `panic = "abort"` must stay off | The hook relies on `catch_unwind`; aborting would turn a panic into a shell crash. |
| The published pipe name is part of the contract | The hook connects to `\\.\pipe\BrightTray`. The name must not change without a coordinated discovery mechanism, which this round does not introduce. |

## Task map

| # | Task | Audit item | Crates | Depends on |
|---|------|-------------|--------|------------|
| 1 | `11-13-p0-01-no-shellhost-kill` | P0-01 | helper, app | – |
| 2 | `11-13-p0-03-no-fake-100-percent` | P0-03 | hook | – |
| 3 | `11-13-p0-04-ddc-write-amplification` | P0-04 | helper | 1 (independent, but ships after) |
| 4 | `11-13-p0-02-pipe-acl-and-ratelimit` | P0-02 | helper | 3 (the bucket refines the throttle) |

The order is the audit's: the user-visible risk first (P0-01), then the
dishonest value (P0-03), then the wear (P0-04), then the exposure (P0-02).

Tasks 1 and 2 are independent and can be done in any order. Task 4 is
implemented last because its token bucket is a refinement of the throttle
introduced in task 3, and the two touch the same worker loop.

## Cross-child acceptance criteria

- [ ] No code path terminates `ShellHost.exe` without an explicit user action.
- [ ] With the helper killed, Quick Settings shows no invented brightness
      value.
- [ ] A 100-step slider drag issues at most a handful of `SetVCPFeature`
      calls, and the final brightness still lands.
- [ ] A process running as a different user cannot connect to
      `\\.\pipe\BrightTray`.
- [ ] A flood of `setall` commands cannot drive the write rate above the cap.
- [ ] The Quick Settings slider still appears, still drives the monitor, and
      the other Quick Settings tiles are unaffected.
- [ ] `brighttray-helper list`, `set`, `serve`, `inject`, `probe` and `watch`
      all still behave as documented in `README.md`.
- [ ] `.trellis/spec/` describes the new behaviour (each child updates its own
      package spec).

## Out of scope

- P1-01 unit tests (deferred by decision).
- P1-02 vtable slot de-duplication and build-number mapping.
- P1-03 `GetSystemDirectoryW` instead of the hard-coded system path.
- P1-04 build-path remapping.
- P1-05 hook log rotation.
- P1-06 atomic `config.ini` write.
- P1-07 incremental hook-DLL comparison.
- All P2 items.

## Notes

The audit's own §5 (verification checklist) and §7 (what the audit did not
cover) are the reference for the acceptance checks. The audit did not verify
the release binary against a local build and did not measure the real crash
rate, so the first task should also collect the slot data the later tasks
depend on.
