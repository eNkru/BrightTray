# Implementation plan — P0-02

## Step 1 — `helper/Cargo.toml`

- [ ] Add `Win32_Security_Authorization` to the `windows` crate's feature
      list in the helper's dependency block.

## Step 2 — `helper/src/server.rs` — the security descriptor

- [ ] Add a helper that returns the current user's SID as a string:
      `OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, …)` →
      `GetTokenInformation(…, TokenUser, …)` → `ConvertSidToStringSidW`.
      Free the SID string with `LocalFree`.
- [ ] Build the SDDL `D:P(A;;GA;;;<SID>)S:(ML;;NW;;;ME)` and turn it into a
      `SECURITY_DESCRIPTOR` with
      `ConvertStringSecurityDescriptorToSecurityDescriptorW` and
      `SDDL_REVISION_1`.
- [ ] Store the descriptor and the `SECURITY_ATTRIBUTES` in a `OnceLock` that
      lives for the process lifetime; leak the descriptor deliberately and
      document why.
- [ ] `security_attributes()` returns `Option<&'static SECURITY_ATTRIBUTES>`;
      `None` when the descriptor could not be built.
- [ ] `create_instance()` passes the attributes to `CreateNamedPipeW`. When
      they are `None`, the call keeps today's behaviour and the server prints
      one warning to stderr — it must still start.
- [ ] The warning must say what is lost: any same-user process can connect.

## Step 3 — `helper/src/server.rs` — the write rate limit

- [ ] Add `const WRITE_RATE_LIMIT: u32 = 10;` with a comment that states the
      invariant it enforces.
- [ ] Add a small per-monitor token bucket: `tokens`, `last_refill`, and a
      `take(now) -> bool` that refills by the elapsed time and consumes one
      token.
- [ ] Keep one bucket per monitor index next to the throttle state.
- [ ] In the worker's write path, take a token **before** the write. If the
      bucket is empty, leave the value pending and skip the write for now —
      do not drop the value.
- [ ] Every write path goes through the same place, including the retry after
      a stale handle and the write that a read forces out.

## Step 4 — `helper/src/server.rs` — the module doc

- [ ] The module header describes the pipe's access rules: who may connect,
      and the write rate.

## Step 5 — Spec

- [ ] `spec/brighttray-helper/backend/pipe-protocol.md` — replace the "Who may
      connect" section with the new rule (only the current user, medium
      integrity label) and document the write cap.
- [ ] `spec/brighttray-helper/backend/windows-ffi.md` — the SDDL /
      `SECURITY_ATTRIBUTES` pattern, the `Win32_Security_Authorization`
      feature, and the "leak the descriptor on purpose" convention.

## Step 6 — Verification (on Windows)

```powershell
cargo build --release
cargo clippy --workspace --all-targets
cargo fmt --check
```

- [ ] `target\release\brighttray-helper.exe serve` — the server starts and
      prints no warning (the descriptor was built).
- [ ] **Same user, normal process:** the hook still connects and the slider
      still works end to end.
- [ ] **Different user:** a process running as another account cannot open
      `\\.\pipe\BrightTray`.
- [ ] **Low integrity:** a low-integrity process cannot open the pipe for
      writing.
- [ ] **Flood:** a script that sends `setall` in a tight loop cannot make the
      monitor be written more than ~10 times per second (count the
      `SetVCPFeature` calls, e.g. from the monitor's own behaviour or an
      instrumented build).
- [ ] **Deferred write:** after the flood stops, the last requested value is
      still applied.
- [ ] **Fallback:** if the descriptor cannot be built (temporarily force the
      failure), the server still starts, prints the warning, and the hook
      still works.

## Rollback

One commit, one revert: pass `None` as the security attributes again and
drop the bucket. The pipe name and the wire format never change, so a revert
cannot leave the two ends disagreeing.
