# Error Handling

The app has no console. Everything the user needs to know has to reach the
window.

## User-facing errors live in `App::error`

`App::error: Option<String>` is rendered by the page's notice area (the red
bar under the display list). It is set with a bilingual `tr()` string:

```rust
self.error = Some(
    tr(
        "这台显示器已断开，请刷新后重试。",
        "This display is no longer connected. Refresh and try again.",
    )
    .into(),
);
```

Rules:

- **Never show a raw Windows error.** Wrap it: the prefix tells the user what
  failed, the cause follows.

  ```rust
  format!("{}{e}", tr("无法启动服务：", "Couldn't start the service: "))
  ```

- **`AddrInUse` gets its own message**, because it has its own fix:

  ```rust
  tr(
      "另一个 BrightTray（例如 brighttray-helper serve）正在运行，请先关闭它。",
      "Another BrightTray (for example brighttray-helper serve) is already running. Close it first.",
  )
  ```

- **Clear the error when the situation changes.** `on_ipc` clears it on
  `refresh`, and the page clears `state.error` when the user picks another
  display.

## Errors that are not the user's fault stay out of the UI

A failed hook injection is not an error banner — it is the hook *status* line.
`hookdll::extract()` failing becomes a `WatchEvent::Error`, which
`push_state()` maps to `{"kind": "error", "message": …}` and the page shows in
the status row:

```rust
Err(e) => self.hook = Hook::Event(WatchEvent::Error(e)),
```

So the app distinguishes:

| Situation | Where it shows up |
|-----------|-------------------|
| the user asked for something impossible (display gone, no DDC/CI) | the notice area (`App::error`) |
| a background step failed (hook not installed, DLL missing) | the status line (`hook.kind`) |

## Refusals are not errors

An unsupported display is never touched:

```rust
// An unsupported monitor is never touched: no server, no hook, no saved choice.
if !monitor.ddc() {
    self.error = Some(tr("这台显示器不支持 DDC/CI，未做任何修改。", …).into());
    self.push_state();
    return;
}
```

No server is started, no hook is injected, nothing is saved. The same applies
to a display that disappeared: the apply is refused and the user is asked to
refresh.

## Fatal startup errors

Two things cannot be recovered from, and the app aborts with a message
instead of showing an empty window:

```rust
.build(&event_loop)
.expect("failed to create the window");
…
.expect("failed to create the WebView2 control");
```

WebView2 not being installed, or the window failing to be created, means the
app cannot run. These are the **only** two `expect()` calls in the crate.

## Silent failure is acceptable for background work

- `Config::save()` ignores write errors — there is nothing the user can do
  about a read-only `%APPDATA%`, and the setting is re-entered on demand.
  The write is a plain `fs::write` of the whole file, so it is not atomic:
  a crash between truncate and write leaves an empty `config.ini`, which
  `load()` then reads as "no monitor saved".
- `Config::load()` returns the defaults when the file is missing or
  unreadable.
- `hookdll::extract()` reports failures through the hook status, not as a
  crash.
- `push_state()` ignores the result of `evaluate_script` — the page may not
  be ready yet, and it will ask for the state with `ready`.
- `autostart::refresh_path()` ignores errors; it runs at every startup and
  only refreshes the path if the exe moved.
