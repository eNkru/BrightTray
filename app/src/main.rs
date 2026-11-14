//! BrightTray desktop app: pick the monitor that the Quick Settings brightness slider should
//! control, apply it, and keep working from the tray.

#![windows_subsystem = "windows"]

mod autostart;
mod config;
mod hookdll;
mod i18n;
mod instance;

use brighttray::ddc;
use brighttray::inject::{self, WatchEvent};
use brighttray::server::{Server, Target};
use serde_json::json;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::{IconExtWindows, WindowBuilderExtWindows, WindowExtWindows};
use tao::window::{Icon as WindowIcon, Theme, Window, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{
    Icon as TrayIconImage, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent,
};
use wry::{WebView, WebViewBuilder};

use config::Config;
use i18n::tr;

const ICON_RESOURCE: u16 = 1;
/// Logical window width; the height follows the page content (see the `size` IPC message).
const WINDOW_WIDTH: f64 = 420.0;
const MIN_HEIGHT: f64 = 200.0;
const MAX_HEIGHT: f64 = 720.0;
/// How many 3-second rescans a logon start waits for the saved monitor (about 30 s).
const RESUME_ATTEMPTS: u32 = 10;
const UI: &str = include_str!("../ui/index.html");
const ICON_SVG: &str = include_str!("../../assets/icon.svg");

#[derive(Debug)]
enum UserEvent {
    Ipc(String),
    Scanned(Vec<MonitorView>),
    Rescan,
    /// Fallback in case the page never reports its size: show the window anyway.
    Reveal,
    Watch(WatchEvent),
    Show,
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    /// The result of the explicit ShellHost restart the user asked for.
    RestartShellHost(Result<(), String>),
}

#[derive(Debug, Clone)]
struct MonitorView {
    key: String,
    name: String,
    device: String,
    width: i32,
    height: i32,
    primary: bool,
    percent: Option<u32>,
}

impl MonitorView {
    fn ddc(&self) -> bool {
        self.percent.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Hook {
    Idle,
    Starting,
    Event(WatchEvent),
}

struct App {
    window: Window,
    webview: WebView,
    proxy: EventLoopProxy<UserEvent>,
    config: Config,
    monitors: Vec<MonitorView>,
    scanning: bool,
    /// Started on the first apply; lives until the process exits.
    server: Option<Server>,
    applied: Option<String>,
    hook: Hook,
    watcher_started: bool,
    error: Option<String>,
    /// Launched by the Run key: resume the saved monitor once the first scan is in.
    resume_pending: bool,
    /// Rescans spent waiting for the saved monitor after logon (DisplayPort monitors can take
    /// a few seconds to appear after wake).
    resume_attempts: u32,
    last_scan: Option<std::time::Instant>,
    /// The window is shown only once the page has laid out and sized it, so it never jumps.
    reveal_pending: bool,
    show_id: MenuId,
    restart_id: MenuId,
    quit_id: MenuId,
}

impl App {
    fn push_state(&self) {
        let monitors: Vec<_> = self
            .monitors
            .iter()
            .map(|m| {
                json!({
                    "key": m.key,
                    "name": m.name,
                    "device": m.device,
                    "width": m.width,
                    "height": m.height,
                    "primary": m.primary,
                    "ddc": m.ddc(),
                    "percent": m.percent.unwrap_or(0),
                })
            })
            .collect();
        let hook = match &self.hook {
            Hook::Idle => json!({ "kind": "idle" }),
            Hook::Starting => json!({ "kind": "starting" }),
            Hook::Event(WatchEvent::Hooked) => json!({ "kind": "hooked" }),
            Hook::Event(WatchEvent::NoShellHost) => json!({ "kind": "waiting" }),
            Hook::Event(WatchEvent::DllMissing(_)) => json!({ "kind": "missing" }),
            Hook::Event(WatchEvent::Error(message)) => {
                json!({ "kind": "error", "message": message })
            }
        };
        let state = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "monitors": monitors,
            "scanning": self.scanning,
            "applied": self.applied,
            "saved": self.config.monitor_key,
            "hook": hook,
            "error": self.error,
            "autostart": autostart::is_enabled(),
        });
        let _ = self
            .webview
            .evaluate_script(&format!("window.bt && window.bt.render({state})"));
    }

    /// Enumerates monitors and probes DDC/CI off the UI thread (each probe can take ~200 ms).
    fn scan(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        self.last_scan = Some(std::time::Instant::now());
        self.push_state();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let views = ddc::enumerate()
                .iter()
                .map(|m| MonitorView {
                    key: m.key(),
                    name: m.name.clone(),
                    // "\\.\DISPLAY1" -> "DISPLAY1"
                    device: m.device.trim_start_matches(r"\\.\").to_string(),
                    width: m.width,
                    height: m.height,
                    primary: m.primary,
                    percent: m.brightness().map(|b| b.percent()),
                })
                .collect();
            let _ = proxy.send_event(UserEvent::Scanned(views));
        });
    }

    /// Finds the saved monitor again after a layout change, by key and then by unique name.
    fn find_saved(&self) -> Option<&MonitorView> {
        let key = self.config.monitor_key.as_deref()?;
        if let Some(m) = self.monitors.iter().find(|m| m.key == key) {
            return Some(m);
        }
        let name = self.config.monitor_name.as_deref()?;
        let mut by_name = self.monitors.iter().filter(|m| m.name == name);
        match (by_name.next(), by_name.next()) {
            (Some(m), None) => Some(m),
            _ => None,
        }
    }

    fn apply(&mut self, key: &str) {
        self.error = None;
        let Some(monitor) = self.monitors.iter().find(|m| m.key == key).cloned() else {
            self.error = Some(
                tr(
                    "这台显示器已断开，请刷新后重试。",
                    "This display is no longer connected. Refresh and try again.",
                )
                .into(),
            );
            self.push_state();
            return;
        };
        // An unsupported monitor is never touched: no server, no hook, no saved choice.
        if !monitor.ddc() {
            self.error = Some(
                tr(
                    "这台显示器不支持 DDC/CI，未做任何修改。",
                    "This display doesn't support DDC/CI. Nothing was changed.",
                )
                .into(),
            );
            self.push_state();
            return;
        }

        let target = Target::Monitor {
            key: monitor.key.clone(),
            name: monitor.name.clone(),
        };
        match &self.server {
            Some(server) => server.set_target(target),
            None => match Server::start(target) {
                Ok(server) => self.server = Some(server),
                Err(e) => {
                    self.error = Some(if e.kind() == std::io::ErrorKind::AddrInUse {
                        tr(
                            "另一个 BrightTray（例如 brighttray-helper serve）正在运行，请先关闭它。",
                            "Another BrightTray (for example brighttray-helper serve) is already running. Close it first.",
                        )
                        .into()
                    } else {
                        format!(
                            "{}{e}",
                            tr("无法启动服务：", "Couldn't start the service: ")
                        )
                    });
                    self.push_state();
                    return;
                }
            },
        }

        if !self.watcher_started {
            self.hook = Hook::Starting;
            match hookdll::extract() {
                Ok(dll) => {
                    // Only now is there a watcher that can inject into a new ShellHost.
                    self.watcher_started = true;
                    let proxy = self.proxy.clone();
                    // The watcher only injects; it never ends a ShellHost process. A shell
                    // that already cached "no brightness" is handled by the tray menu's
                    // explicit restart.
                    inject::watch_and_inject(dll, move |event| {
                        let _ = proxy.send_event(UserEvent::Watch(event.clone()));
                    });
                }
                Err(e) => self.hook = Hook::Event(WatchEvent::Error(e)),
            }
        }

        self.applied = Some(monitor.key.clone());
        self.config.monitor_key = Some(monitor.key);
        self.config.monitor_name = Some(monitor.name);
        self.config.save();
        self.push_state();
    }

    fn show(&mut self) {
        self.window.set_visible(true);
        self.window.set_minimized(false);
        self.window.set_focus();
        self.scan();
    }

    /// Closing the window always keeps BrightTray running in the tray; "退出" in the tray menu
    /// is the way out.
    fn hide(&self) {
        self.window.set_visible(false);
    }

    /// Ends every running ShellHost so that Windows starts a fresh one, which asks Quick
    /// Settings about the brightness again. The user asked for this by picking the tray menu
    /// item, so it is the one place the app is allowed to end a shell process.
    ///
    /// The watcher injects into the new ShellHost on its own; this only has to start it.
    fn restart_shellhost(&mut self) {
        if !self.watcher_started {
            self.error = Some(
                tr(
                    "请先应用一个显示器，再重启 ShellHost。",
                    "Apply a display first, then restart ShellHost.",
                )
                .into(),
            );
            self.show_notice();
            self.push_state();
            return;
        }
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = inject::restart_shellhost();
            let _ = proxy.send_event(UserEvent::RestartShellHost(result));
        });
    }

    /// Brings the window forward. The app usually lives in the tray, so a notice that only
    /// lives in the page would never be seen.
    fn show_notice(&self) {
        self.window.set_visible(true);
        self.window.set_minimized(false);
        self.window.set_focus();
    }

    fn reveal(&mut self) {
        if self.reveal_pending {
            self.reveal_pending = false;
            self.window.set_visible(true);
            self.window.set_focus();
        }
    }

    fn fit_height(&mut self, height: &str) {
        if let Ok(height) = height.parse::<f64>() {
            let height = height.clamp(MIN_HEIGHT, MAX_HEIGHT);
            self.window
                .set_inner_size(LogicalSize::new(WINDOW_WIDTH, height));
        }
        self.reveal();
    }

    /// Returns false when the app should exit.
    fn on_ipc(&mut self, message: &str) -> bool {
        let (command, argument) = message.split_once(':').unwrap_or((message, ""));
        match command {
            "ready" => {
                self.push_state();
                self.scan();
            }
            "refresh" => {
                self.error = None;
                self.scan();
            }
            "apply" => self.apply(argument),
            "autostart" => {
                if let Err(e) = autostart::set_enabled(argument == "on") {
                    self.error = Some(e);
                }
                self.push_state();
            }
            "size" => self.fit_height(argument),
            "minimize" => self.window.set_minimized(true),
            "close" => self.hide(),
            "drag" => {
                let _ = self.window.drag_window();
            }
            _ => {}
        }
        true
    }
}

fn round_corners(window: &Window) {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    };
    let hwnd = HWND(window.hwnd() as _);
    let preference = DWMWCP_ROUND;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &preference as *const _ as *const _,
            std::mem::size_of_val(&preference) as u32,
        );
    }
}

fn tray_icon_image() -> Option<TrayIconImage> {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
    TrayIconImage::from_resource(ICON_RESOURCE, Some((size, size))).ok()
}

fn main() {
    let Some(_primary) = instance::acquire() else {
        return;
    };
    let started_by_logon = std::env::args().any(|a| a == autostart::FLAG);
    autostart::refresh_path();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("BrightTray")
        .with_inner_size(LogicalSize::new(WINDOW_WIDTH, 320.0))
        .with_resizable(false)
        .with_maximizable(false)
        .with_decorations(false)
        .with_undecorated_shadow(true)
        .with_visible(false)
        .with_window_icon(WindowIcon::from_resource(ICON_RESOURCE, None).ok())
        .build(&event_loop)
        .expect("failed to create the window");
    round_corners(&window);

    let ipc_proxy = proxy.clone();
    let webview = WebViewBuilder::new()
        .with_html(
            UI.replace("{{ICON}}", ICON_SVG)
                .replace("{{LANG}}", i18n::lang().code()),
        )
        // Opaque on purpose: a transparent WebView2 does not clear what it painted before, so
        // changed text (the brightness percentage) left a ghost of the old value behind.
        .with_background_color(if window.theme() == Theme::Dark {
            (0x14, 0x14, 0x14, 0xff)
        } else {
            (0xf4, 0xf4, 0xf4, 0xff)
        })
        .with_devtools(cfg!(debug_assertions))
        .with_ipc_handler(move |request| {
            let _ = ipc_proxy.send_event(UserEvent::Ipc(request.body().clone()));
        })
        .build(&window)
        .expect("failed to create the WebView2 control");

    let menu = Menu::new();
    let show_item = MenuItem::new(tr("打开 BrightTray", "Open BrightTray"), true, None);
    let restart_item = MenuItem::new(
        tr(
            "重启 ShellHost（开始菜单会短暂消失）",
            "Restart ShellHost (the Start menu disappears briefly)",
        ),
        true,
        None,
    );
    let quit_item = MenuItem::new(tr("退出", "Quit"), true, None);
    let _ = menu.append_items(&[
        &show_item,
        &PredefinedMenuItem::separator(),
        &restart_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ]);
    let mut tray_builder = TrayIconBuilder::new()
        .with_tooltip("BrightTray")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false);
    if let Some(icon) = tray_icon_image() {
        tray_builder = tray_builder.with_icon(icon);
    }
    let tray = tray_builder.build().ok();

    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));
    let tray_proxy = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tray_proxy.send_event(UserEvent::Tray(event));
    }));
    let show_proxy = proxy.clone();
    instance::listen(move || {
        let _ = show_proxy.send_event(UserEvent::Show);
    });

    let mut app = App {
        window,
        webview,
        proxy,
        config: Config::load(),
        monitors: Vec::new(),
        scanning: false,
        server: None,
        applied: None,
        hook: Hook::Idle,
        watcher_started: false,
        error: None,
        resume_pending: started_by_logon,
        resume_attempts: 0,
        last_scan: None,
        reveal_pending: !started_by_logon,
        show_id: show_item.id().clone(),
        restart_id: restart_item.id().clone(),
        quit_id: quit_item.id().clone(),
    };
    // Scan right away instead of waiting for the page: a logon start never shows the window.
    app.scan();
    if !started_by_logon {
        let proxy = app.proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let _ = proxy.send_event(UserEvent::Reveal);
        });
    }

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        // Keep the tray icon alive for as long as the loop runs.
        let _ = &tray;

        match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => app.hide(),
                // Monitors come and go (sleep, hot-plug, remote sessions): refresh when the
                // user comes back to the window, at most every few seconds.
                WindowEvent::Focused(true) => {
                    if app.last_scan.is_none_or(|t| t.elapsed().as_secs() >= 3) {
                        app.scan();
                    }
                }
                _ => {}
            },
            Event::UserEvent(event) => match event {
                UserEvent::Ipc(message) => {
                    if !app.on_ipc(&message) {
                        *control_flow = ControlFlow::Exit;
                    }
                }
                UserEvent::Scanned(monitors) => {
                    app.monitors = monitors;
                    app.scanning = false;
                    if app.resume_pending {
                        match app.find_saved().filter(|m| m.ddc()).map(|m| m.key.clone()) {
                            Some(key) => {
                                app.resume_pending = false;
                                app.apply(&key);
                            }
                            // Right after logon the monitor may still be waking up.
                            None if app.config.monitor_key.is_some()
                                && app.resume_attempts < RESUME_ATTEMPTS =>
                            {
                                app.resume_attempts += 1;
                                let proxy = app.proxy.clone();
                                std::thread::spawn(move || {
                                    std::thread::sleep(std::time::Duration::from_secs(3));
                                    let _ = proxy.send_event(UserEvent::Rescan);
                                });
                            }
                            // Nothing to resume: ask the user instead of idling invisibly.
                            None => {
                                app.resume_pending = false;
                                if app.config.monitor_key.is_some() {
                                    app.error = Some(
                                        tr(
                                            "上次应用的显示器当前不可用，请重新选择。",
                                            "The display you applied last time isn't available. Pick one again.",
                                        )
                                        .into(),
                                    );
                                }
                                app.show();
                            }
                        }
                    }
                    app.push_state();
                }
                UserEvent::Watch(event) => {
                    app.hook = Hook::Event(event);
                    app.push_state();
                }
                UserEvent::Rescan => app.scan(),
                UserEvent::Reveal => app.reveal(),
                UserEvent::Show => app.show(),
                UserEvent::Menu(event) => {
                    if event.id == app.quit_id {
                        *control_flow = ControlFlow::Exit;
                    } else if event.id == app.show_id {
                        app.show();
                    } else if event.id == app.restart_id {
                        app.restart_shellhost();
                    }
                }
                UserEvent::RestartShellHost(result) => {
                    // On success the hook state is left alone on purpose: the watcher
                    // reports the new ShellHost on its own, and setting it here would race
                    // with that report.
                    if let Err(e) = result {
                        app.error = Some(format!(
                            "{}{e}",
                            tr("无法重启 ShellHost：", "Couldn't restart ShellHost: ")
                        ));
                        app.show_notice();
                    }
                    app.push_state();
                }
                UserEvent::Tray(TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                })
                | UserEvent::Tray(TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }) => app.show(),
                UserEvent::Tray(_) => {}
            },
            _ => {}
        }
    });
}
