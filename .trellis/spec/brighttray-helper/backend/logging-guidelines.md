# Logging Guidelines

## There is no logging framework

The helper crate has **no** `log`, `tracing`, `env_logger` or any other
logging dependency, and it must not grow one. Diagnostics are plain writes to
the process's own streams:

| Stream | Used for | API |
|--------|----------|-----|
| stdout | the result the user asked for (`list`, `set`, `watch` values) | `println!` |
| stderr | problems and progress notices | `eprintln!` |

Examples from `helper/src/main.rs`:

```rust
println!("[{index}] {} -> {}%", m.name, percent.min(100));   // the result
eprintln!("inject failed: {e}");                              // the problem
eprintln!("hook watcher: {event}");                           // progress
```

## Report a state change once

The hook watcher polls ShellHost forever. It must not print on every tick.
`inject::watch_and_inject` keeps the last `WatchEvent` and only calls the
`report` callback when the observed state differs:

```rust
if last.as_ref() != Some(&event) {
    report(&event);
    last = Some(event);
}
```

`WatchEvent` implements `Display`, so the CLI prints the event directly. When
you add a variant to `WatchEvent`, add the `Display` arm too — otherwise the
watcher cannot report it.

## The hook DLL has its own log file

`%TEMP%\brighttray-hook.log` is written by the hook, not by the helper. The
helper never writes it, and the app never writes it. See the hook package's
[Logging Guidelines](../../brighttray-hook/backend/logging-guidelines.md).

## What not to log

- No user data beyond monitor names and brightness percentages.
- No file contents, no registry values.
- Keep the output short: the CLI is used interactively, and `serve` runs for
  hours.

## Long-running output

`brighttray-helper serve` prints hook watcher events to stderr while the pipe
server runs. It does **not** print per-command traffic — the pipe protocol is
silent by design.
