# UI Contract

The page and Rust talk over `window.ipc.postMessage`. Every message is a
single string, `command:argument`. There is no JSON on the way *in*; JSON is
only used on the way *out*.

## Page → Rust

The page sends with `const send = (msg) => window.ipc.postMessage(msg);`.

| Message | Argument | Effect |
|---------|----------|--------|
| `ready` | – | the page has loaded: push the state and start a scan |
| `refresh` | – | rescan the displays |
| `apply:<key>` | monitor key | apply the brightness slider to that display |
| `autostart:on` / `autostart:off` | `on`/`off` | toggle the Run key |
| `size:<px>` | height in CSS px | resize the window to the content height |
| `minimize` | – | minimize |
| `close` | – | hide to the tray |
| `drag` | – | drag the window by the title bar |

Rust parses them in `App::on_ipc`:

```rust
let (command, argument) = message.split_once(':').unwrap_or((message, ""));
```

An unknown command falls through the `_ => {}` arm and is ignored.

## Rust → page

Rust pushes the whole state as JSON and calls the page's render function:

```rust
let _ = self
    .webview
    .evaluate_script(&format!("window.bt && window.bt.render({state})"));
```

`state` is built with `serde_json::json!`, so it is escaped as JSON — never
build this string by hand.

### State schema

```jsonc
{
  "version": "0.1.0",            // env!("CARGO_PKG_VERSION")
  "monitors": [{
    "key":   "\\.\DISPLAY1#0|Lenovo Y27q-30",  // identity used by "apply"
    "name":  "Lenovo Y27q-30",
    "device": "DISPLAY1",        // "\\.\DISPLAY1" without the prefix
    "width": 2560, "height": 1440,
    "primary": true,
    "ddc": true,                 // a brightness read answered
    "percent": 42                // 0 when unknown
  }],
  "scanning": false,              // a scan is running
  "applied": "<key>",             // the display the slider drives, or null
  "saved":   "<key>",             // the display saved in config.ini, or null
  "hook": { "kind": "hooked" },   // see below
  "error": null,                  // a user-facing message, or null
  "autostart": true               // the Run key exists
}
```

`hook.kind` maps 1:1 onto `brighttray::inject::WatchEvent`:

| `WatchEvent` | `kind` | page text |
|--------------|--------|-----------|
| – | `idle` | "未启用" / "Not active" |
| – (app internal) | `starting` | "正在挂接快速设置…" |
| `Hooked` | `hooked` | "运行中" / "Running" |
| `NoShellHost` | `waiting` | "等待 ShellHost…" |
| `DllMissing` | `missing` | "找不到 brighttray_hook.dll" |
| `Error(msg)` | `error` (+ `message`) | the message |

**Adding a `WatchEvent` variant means changing three places**: the match in
`App::push_state()`, the `switch` in the page's `statusOf()`, and the string
table. The page's `switch` has a `default` arm, so a missing variant shows
"Starting…" instead of breaking — but it is still a bug.

## Who owns what

- **Rust owns the state.** The page renders what it is given.
- **The page owns the selection.** It keeps `selected` locally and sends
  `apply:<key>`; Rust re-validates the key against the current monitor list
  before doing anything, because the page's list can be stale.
- **The page owns the layout.** It measures itself and sends `size:<px>`.
- **The page never computes brightness.** `percent` comes from the state.

## The size handshake

The window is created hidden, so it never jumps:

```rust
/// The window is shown only once the page has laid out and sized it, so it never jumps.
reveal_pending: !started_by_logon,
```

The page sends its height whenever it changes; Rust clamps it and then
reveals the window:

```rust
fn fit_height(&mut self, height: &str) {
    if let Ok(height) = height.parse::<f64>() {
        let height = height.clamp(MIN_HEIGHT, MAX_HEIGHT);
        self.window.set_inner_size(LogicalSize::new(WINDOW_WIDTH, height));
    }
    self.reveal();
}
```

`MIN_HEIGHT = 200.0`, `MAX_HEIGHT = 720.0`, `WINDOW_WIDTH = 420.0`. The page
also re-fits on any layout change with a `ResizeObserver`, so a font load or a
DPI change is picked up.

## Scanning

`App::scan()` spawns a thread that enumerates the monitors and probes them
(one probe is ~200 ms), then reports back with `UserEvent::Scanned`. The
event loop stores the result and calls `push_state()`.

The page shows a skeleton while `scanning` is true and there are no monitors
yet, and it only disables the refresh button — the Apply button stays
clickable during a background rescan, because the click that focused the
window must not be swallowed.

## Validation and error matrix

| Page message | Condition | What happens |
|--------------|-----------|--------------|
| `ready` | the page has loaded | the state is pushed and a scan starts |
| `refresh` | – | the error is cleared and a rescan starts |
| `apply:<key>` | the key is in the monitor list | the server is started or retargeted, the hook watcher starts on the first apply, the choice is saved |
| `apply:<key>` | the key is unknown | the notice shows "display is no longer connected"; **nothing else changes** — no server, no hook, nothing saved |
| `apply:<key>` | the display does not support DDC/CI | the notice shows "doesn't support DDC/CI"; no server, no hook, nothing saved |
| `autostart:on` | – | the Run entry is written; a failure becomes the notice |
| `autostart:off` | – | the Run entry is deleted; a failure becomes the notice |
| `size:<px>` | the argument parses as `f64` | the window is resized to the clamped height and revealed |
| `size:<px>` | the argument does not parse | the size is ignored, but the window is still revealed |
| `minimize` | – | the window is minimized |
| `close` | – | the window is hidden — the app stays in the tray |
| `drag` | – | the window is dragged by the title bar |
| anything else | – | ignored (`_ => {}` in `on_ipc`) |

## Good / base / bad cases

- **Good** — the user picks a display and presses Apply: the server starts,
  the hook is injected, the status line goes to "运行中", and the slider
  appears in Quick Settings.
- **Base** — the page is still loading: `push_state()` is called with an empty
  monitor list, the page renders the "no displays" state, and the scan that
  arrives a moment later replaces it.
- **Bad** — the user presses Apply on a display that was unplugged: the key
  is not in the list, the notice explains, and the app stays exactly as it
  was. The page's own selection is stale — Rust is the one that decides.

## Wrong vs correct

Text that comes from a monitor name must never reach `innerHTML` raw:

```javascript
// Wrong: a monitor named `<img onerror=…>` executes in the page.
return `<div class="name">${m.name}</div>`;
```

```javascript
// Correct: escape it, the same way the rest of the page does.
return `<div class="name"><span class="t">${esc(m.name)}</span></div>`;
```

And on the Rust side, never build the state by string concatenation:

```rust
// Wrong: the name breaks out of the JSON literal.
let script = format!("window.bt.render({{\"name\":\"{}\"}})", monitor.name);
```

```rust
// Correct: serde_json escapes it.
let state = json!({ "monitors": monitors });
let _ = self.webview.evaluate_script(&format!("window.bt && window.bt.render({state})"));
```

## Apply

`App::apply(key)` refuses in two cases, both with a bilingual message and
without changing anything:

- the display is gone → "这台显示器已断开，请刷新后重试。"
- the display does not support DDC/CI → "这台显示器不支持 DDC/CI，未做任何修改。"

Otherwise it starts (or retargets) the `Server`, starts the hook watcher on
the first apply, and saves `monitor_key` + `monitor_name` in `config.ini`.
