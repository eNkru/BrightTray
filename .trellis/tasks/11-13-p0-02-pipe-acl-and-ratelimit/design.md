# Design — P0-02

## Part 1 — Who may connect

### The current situation

`CreateNamedPipeW` is called with `lpSecurityAttributes = NULL`. The kernel
then applies the default security descriptor of the creating token, whose DACL
grants access to the user, to SYSTEM and to the administrators group. Every
process running as that user — a browser helper, an editor plugin, anything
the user starts — can open `\\.\pipe\BrightTray`.

`PIPE_REJECT_REMOTE_CLIENTS` rejects clients that arrive over the network. It
does nothing about local processes.

### The descriptor

The descriptor is built from the running process's own token, so it is
correct for any account:

1. `OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)`
2. `GetTokenInformation(token, TokenUser, …)` → the user's SID
3. `ConvertSidToStringSidW(sid)` → `"S-1-5-21-…"`
4. The SDDL string is assembled from that SID
5. `ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, SDDL_REVISION_1,
   &mut descriptor, &mut size)` → the `SECURITY_DESCRIPTOR`

The SDDL:

```
D:P(A;;GA;;;<user SID>)S:(ML;;NW;;;ME)
```

| Part | Meaning |
|------|---------|
| `D:P` | a protected DACL — no inherited ACEs from the parent |
| `(A;;GA;;;<SID>)` | allow generic all to that SID only |
| `S:(ML;;NW;;;ME)` | a mandatory label: medium integrity, no write-up |

`GA` is generic all. There is deliberately **no** ACE for SYSTEM, the
administrators group, or everyone — the pipe belongs to the user who started
BrightTray, and to nobody else.

The mandatory label is explicit rather than implicit on purpose: a descriptor
with an explicit DACL and no SACL section can end up with no integrity
protection at all, depending on how the object manager interprets it. Naming
the label removes the question.

The label is fixed at medium because BrightTray is a user-level application;
the helper always runs at medium integrity. An elevated helper can still
write to a medium-labelled object (write-down is allowed).

### Lifetime and plumbing

The `SECURITY_ATTRIBUTES` must outlive every `CreateNamedPipeW` call, and the
accept loop creates a new instance for every client. So the descriptor is
built **once** and kept for the process lifetime:

```rust
/// The pipe's security descriptor, built once and kept for the lifetime of
/// the process: every pipe instance must use the same one.
fn security_attributes() -> Option<&'static SECURITY_ATTRIBUTES>
```

It is a `OnceLock` around a leaked `SECURITY_DESCRIPTOR`. Leaking it is
deliberate: the descriptor must stay valid until the process exits, and the
process is the only owner.

`create_instance()` passes it as the last argument of `CreateNamedPipeW`.
When it is `None` (the descriptor could not be built), the call keeps the
current behaviour and the server prints a warning to stderr — it must never
fail to start because of a security detail.

### The `windows` crate features

Two APIs are needed that the helper does not use today, both in
`Win32_Security_Authorization`:

- `ConvertStringSecurityDescriptorToSecurityDescriptorW`
- `ConvertSidToStringSidW`

`Win32_Security` and `Win32_System_Threading` (for `OpenProcessToken` and
`GetTokenInformation`) are already enabled in `helper/Cargo.toml`. The new
feature is added to the helper's own `windows` dependency.

### Why the name stays predictable

The audit also suggests a random pipe name plus a discovery file in
`%APPDATA%`. That is a bigger change than the problem needs:

- A hook that is already loaded in ShellHost would not find the new name
  without a discovery mechanism, so it breaks the "the hook keeps working"
  requirement.
- The name is not the vulnerability. The access control is. A predictable name
  behind a correct DACL is not reachable by anyone else.

## Part 2 — How fast writes may reach the monitor

### Two different controls

P0-04 introduced a **throttle**: two writes to the same monitor are at least
`WRITE_INTERVAL` apart. That is a UX control — it keeps the write cadence
close to the cadence of the user's input.

The audit's concern is different: a connected process must not be able to
drive the monitor at all. The throttle alone does not guarantee that, because
some paths intentionally bypass it:

- a read (`get` / `getall`) forces a pending write out, ignoring the
  throttle window;
- the retry after a stale handle writes again immediately.

So this task adds an **invariant**: no monitor is written more than
`WRITE_RATE_LIMIT` times per second, whichever path asks for the write.

### The bucket

Per monitor, a small token bucket:

```rust
/// How many monitor writes per second are allowed, per monitor.
const WRITE_RATE_LIMIT: u32 = 10;
```

The bucket holds `WRITE_RATE_LIMIT` tokens and refills at
`WRITE_RATE_LIMIT` tokens per second. A write takes one token; if the bucket
is empty, the write is **deferred** — the value stays pending and is written
at the next opportunity.

Per monitor rather than global: a legitimate setup with three monitors writes
to three monitors at once, and a global budget would throttle that. The
number of monitors is small and bounded, so a per-monitor budget still bounds
the total.

### Where the bucket is applied

In the worker's write path — the single place where a monitor write happens.
The check and the take happen together, so the accounting cannot drift:

```rust
if !bucket.take(now) {
    // The budget is exhausted: keep the value pending and try later.
    continue;
}
```

The deferred value is not lost: it stays in `pending` and the next tick
writes it, once the budget has refilled.

### Interaction with the throttle

| Situation | Throttle | Bucket |
|-----------|----------|--------|
| A normal drag (one step every 50 ms) | one write per 300 ms | ~3 writes/s, under the cap |
| A burst of 100 sets | one write, then one trailing write | far under the cap |
| A `set` + `get` flood | the read forces the write out | **capped at 10/s** |
| A stale-handle retry | writes again immediately | **capped at 10/s** |

The bucket is what makes the cap hold in the last two rows.

## Summary of the changes

| File | Change |
|------|--------|
| `helper/Cargo.toml` | add the `Win32_Security_Authorization` feature |
| `helper/src/server.rs` | build the security descriptor; pass it to `CreateNamedPipeW`; add the per-monitor token bucket to the write path |
| `hook/src/*` | unchanged |
| `app/src/*` | unchanged |

## Risks

| Risk | Mitigation |
|------|------------|
| The SDDL string is wrong and the hook cannot connect | The build of the descriptor is verified before the first pipe instance is created; a failure falls back to the current behaviour with a warning, and the hook's connection is the first thing to check on Windows |
| The mandatory label is too strict for an elevated helper | Write-down is allowed, so an elevated helper can still write to a medium-labelled pipe |
| The bucket drops a value | It does not drop: the value stays pending and is written later |
| A legitimate multi-monitor drag is throttled | The budget is per monitor and generous compared with the 3.3 writes/s a drag produces |
