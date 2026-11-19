# Logging Guidelines

The hook has exactly one log: `%TEMP%\brighttray-hook.log`. It is the only
place where the user can see what the hook did, and the README FAQ points
people at it ("check `%TEMP%\brighttray-hook.log` for
`IQuickAction hook installed`").

## Format

One line per event:

```
[<unix seconds> pid=<pid>] <message>
```

Example:

```
[1762910400 pid=8124] brighttray_hook attached version=0.1.2 build=4f2c1ab…
[1762910400 pid=8124] IQuickAction hook installed: build=26100 vtable=0x7FF6A1B40000 action=0x7FF6A1D2E118 slots=7,8,9,10,12,14,19,20,21,22
[1762910402 pid=8124] QuickSetValue("Value")
```

The `version=`/`build=` suffix comes from `hook/build.rs` (`CARGO_PKG_VERSION` and
`BRIGHTTRAY_BUILD_ID`, the commit in CI, `dev` otherwise), so the first line of the log names
the DLL that was injected. It is the only place that identity is visible at runtime.

## Logging must never be able to take down ShellHost

```rust
fn append_line(path: &std::path::Path, secs: u64, message: &str) {
    use std::io::Write;

    // Before the open, so a rotation can never split the line that is about to be written.
    rotate_if_needed(path);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(file, "[{secs} pid={}] {message}", std::process::id());
}

fn log(message: impl AsRef<str>) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    append_line(
        &std::env::temp_dir().join("brighttray-hook.log"),
        secs,
        message.as_ref(),
    );
}
```

If the file cannot be opened, `append_line()` returns silently. There is no
error path, no panic, no retry. The hook runs inside a system process; a
logging failure must not become a shell failure. `rotate_if_needed()` obeys
the same rule - see [The log is capped](#the-log-is-capped).

## De-duplicate the noisy events

The shell calls `GetValue` and `SetValue` constantly. `log_property()` writes
each `operation:name` pair only once:

```rust
fn log_property(operation: &str, name: &str) {
    let seen = SEEN_PROPERTIES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = seen.lock().unwrap_or_else(|e| e.into_inner());
    if guard.insert(format!("{operation}:{name}")) {
        log(format!("{operation}(\"{name}\")"));
    }
}
```

The pair includes the operation, so `GetValue("Value")` and
`QuickGetValue("Value")` are logged separately — they come from different
layers and matter for diagnosis.

## Rate-limit the install retries

The install loop runs 30 times, one second apart. Logging every failure would
bury the useful lines, so only attempt 1 and every fifth attempt are logged:

```rust
let report = attempt == 1 || attempt % 5 == 0;
```

Keep that rule when you touch the loop.

## The identifier dump is intentional

```rust
log(format!(
    "IQuickAction hook installed: build={} vtable=0x{:X} action=0x{:X} slots={}",
    windows_build(), vtable as usize, action as usize, slot_list(&patched)
));
```

The vtable and object addresses are what makes a crash report analysable
against the module image. They are process-local addresses, not user data.
The build number and the slot list are the other half: a Windows update that
shifts a table shows up as a changed `build=` / `slots=` line, before any
failure, so the log alone says whether the constants still match.

## The log is capped

`brighttray-hook.log` is bounded: one current file plus one previous
generation, so the hook's footprint in `%TEMP%` is at most two times the cap
no matter how long the machine has been running. `LOG_CAP` in `lib.rs` is
`1 << 20` (1 MiB); at the observed line rate that is thousands of sessions'
worth of lines.

`log()` checks the size before opening the file and rotates it when it reaches
the cap:

```rust
fn rotate_if_needed(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < LOG_CAP {
        return;
    }
    let previous = path.with_extension("log.1");
    // Windows has no rename-over, so the old generation has to go first.
    let _ = std::fs::remove_file(&previous);
    let _ = std::fs::rename(path, &previous);
}
```

What the code guarantees:

- **One previous generation, not a set.** `brighttray-hook.log.1` is removed
  before the rename, so a second rotation replaces it instead of producing
  `.2`. There are never more than the two files.
- **Rotation, not truncation.** The last lines before a ShellHost restart - the
  identifier dump, the last brightness transition, a panic line - are exactly
  what a diagnosis needs, so they move to `.1` instead of being dropped.
  Truncating at the cap would throw them away on a schedule nobody can predict.
- **Best effort, like the write.** Every filesystem error in the rotation path
  is ignored (`let _ = ...`, or an early return from `let Ok(...) else`). A
  read-only `%TEMP%`, a missing directory, a `.log.1` held open by an antivirus
  scanner, or a `.log.1` the user deleted all degrade into "this rotation did
  not happen". No `unwrap`, no `expect`, no `panic!`, no retry, no error line -
  the same rule as the failed `open()` in the section above.
- **A line is never split.** The size check and the rotation run at the top of
  `log()`, before the `OpenOptions` call, so a rotation cannot land between the
  `open()` and the `writeln!()` of one line.
- **The check is one `metadata()`.** No reading the file, no scanning for
  newlines. An in-memory byte counter would be cheaper but wrong: the hook is
  re-injected on every ShellHost start, so a fresh instance cannot see what an
  earlier one wrote.
- **No lock.** Two shell processes can be alive at once, so both instances may
  rotate concurrently, and an unlucky interleaving can lose one line or leave a
  fragment at the boundary. That is acceptable for a diagnostic log nobody
  parses programmatically; a shell-wide named mutex would buy no guarantee a
  reader uses.
- **The format does not change.** Rotation never writes a marker line and never
  rewrites existing lines: `[<unix seconds> pid=<pid>] <message>` stays as it
  is, and a reader of `brighttray-hook.log` sees the same thing as before.

## What is not logged

- No monitor names, no serial numbers, no EDID content — the DLL does not
  know them.
- No pipe traffic: the helper owns that side of the story.
- No per-poll messages from the watcher. The watcher only logs an actual
  change:

  ```rust
  log(format!("external brightness change {previous}% -> {percent}%"));
  ```

  The very first measurement is not a change - there was no previous value -
  so it is logged once as `brightness known: N%`.
