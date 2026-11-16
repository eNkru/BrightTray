//! BrightTray core: DDC/CI access, the `\.\pipe\BrightTray` server and the ShellHost hook
//! injector. Shared by the `brighttray-helper` CLI and the `BrightTray` desktop app.

pub mod ddc;
pub mod inject;
pub mod osver;
pub mod probe;
pub mod server;
pub mod slider;
