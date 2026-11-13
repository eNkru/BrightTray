<div align="center">

<img src="assets/banner.zh-CN.png" alt="BrightTray" width="100%">

<br>
<br>

[English](README.md) · **简体中文**

<br>

<img alt="Windows 11" src="https://img.shields.io/badge/Windows%2011-24H2+-2b2b2b?style=flat-square&labelColor=555555&logo=windows11&logoColor=white">
<img alt="Rust" src="https://img.shields.io/badge/Rust-x64-2b2b2b?style=flat-square&labelColor=555555&logo=rust&logoColor=white">
<img alt="DDC/CI" src="https://img.shields.io/badge/DDC%2FCI-VCP%200x10-2b2b2b?style=flat-square&labelColor=555555">
<img alt="无需管理员" src="https://img.shields.io/badge/admin-not%20required-2b2b2b?style=flat-square&labelColor=555555">

<br>
<br>

<img src="docs/screenshots/demo.png" alt="在 BrightTray 中点击应用后，快速设置里出现亮度条" width="760">

</div>

<br>

台式机的快速设置里没有亮度条。BrightTray 让系统原生的亮度条重新出现，并通过 DDC/CI 调节外接显示器。

## 特性

- **原生** — 就是 Windows 自己的亮度条，不是悬浮窗或托盘弹窗
- **双向同步** — 在显示器 OSD 上改了亮度，滑块会跟着变
- **安全** — 不写死任何偏移；每次打补丁前都先校验，不符就放弃
- **轻量** — 无需管理员权限，不改动系统文件，常驻托盘

## 快速开始

需要 Windows 11 24H2 或更新版本，以及开启了 DDC/CI 的显示器。

```powershell
git clone https://github.com/cestbon0309/BrightTray.git
cd BrightTray
cargo build --release
```

运行 `target\release\BrightTray.exe`，选中显示器，点击 **应用**，然后按 <kbd>Win</kbd> + <kbd>A</kbd>。

`BrightTray.exe` 是独立的单文件，可以拷贝到任意位置使用。

## 工作原理

```mermaid
flowchart LR
    QS["快速设置"] --> HOOK["brighttray_hook.dll<br/>位于 ShellHost.exe"]
    HOOK -- "命名管道" --> HELPER["BrightTray"]
    HELPER -- "DDC/CI" --> MON["显示器"]
```

注入 `ShellHost.exe` 的 hook 让被隐藏的亮度磁贴生效，并把数值经命名管道转发出去，再由 helper 写入显示器。完整的逆向分析见 [docs/quick-settings-hook-research.md](docs/quick-settings-hook-research.md)。

## 命令行

<details>
<summary><code>brighttray-helper &lt;command&gt;</code></summary>

<br>

| 命令 | 说明 |
|---|---|
| `list` | 列出显示器及其 DDC/CI 亮度 |
| `set <index> <0-100>` | 设置指定显示器的亮度 |
| `serve` | 启动管道服务并注入 hook，ShellHost 重启后自动重新注入 |
| `inject [--early] [dll]` | 手动注入 hook |
| `probe` | 只读的兼容性检查 |
| `watch` | 打印 Windows 亮度滑块的变化 |

`serve` 会控制所有支持 DDC/CI 的显示器，不能和桌面应用同时运行。与桌面应用不同，helper 需要 `brighttray_hook.dll` 放在它旁边。

</details>

<details>
<summary>管道协议 — <code>\\.\pipe\BrightTray</code></summary>

<br>

每行一条命令，仅接受本机连接。

| 命令 | 回复 |
|---|---|
| `list` | 每台显示器一行 `index\tcurrent\tmax\tdescription`，以空行结束 |
| `get <index>` | `current max`，失败时返回 `err` |
| `set <index> <0-100>` | 无 |
| `getall` | 第一台支持 DDC/CI 的显示器的亮度百分比，失败时返回 `err` |
| `setall <0-100>` | 无 |

</details>

## 常见问题

<details>
<summary>亮度条没有出现</summary>

<br>

先运行 `brighttray-helper list`，确认显示器能读到亮度；读不到时请在显示器 OSD 里开启 DDC/CI。然后查看 `%TEMP%\brighttray-hook.log`，其中应有 `IQuickAction hook installed`。

</details>

<details>
<summary>杀毒软件报警</summary>

<br>

BrightTray 通过 `CreateRemoteThread` 把 DLL 加载进 `ShellHost.exe`，部分安全软件会把这种行为视为可疑。源码都在这里，可以自行审阅并构建。

</details>

<details>
<summary>如何卸载</summary>

<br>

关闭 **开机自启动**，在托盘菜单中选择 **退出**，然后注销或重启 ShellHost。最后删除 `%APPDATA%\BrightTray`，里面是设置和释放出的 hook。

</details>

## 免责声明

BrightTray 会修改 Windows Shell 未公开的内部接口，系统更新可能导致它失效。使用风险自负。

<br>

<div align="center">
<img src="assets/icon-256.png" width="48" alt="">
</div>
