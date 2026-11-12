# BrightTray

Windows 快速设置（Win+A）亮度条 + 外接显示器 DDC/CI 亮度控制。

Windows 在没有内置屏的台式机上会隐藏亮度条。这个项目由两部分组成：

- `helper/`：常驻进程，负责 DDC/CI 读写、命名管道服务，并在 ShellHost 启动/重启后自动注入 hook。
- `hook/`：注入 `ShellHost.exe` 的 Rust DLL，给 `SettingsHandlers_PCDisplay.dll` 的
  `ISettingItem` 和 `QuickActionsDataModel.dll` 的 `QuickSetting`（`IQuickAction`）打 vtable
  补丁，让快速设置的亮度条出现，并把拖动转发给 helper。

技术细节和实测记录见 [docs/quick-settings-hook-research.md](docs/quick-settings-hook-research.md)。

## 构建

```powershell
cargo build --release
```

产物：

```
target/release/brighttray-helper.exe
target/release/brighttray_hook.dll
```

两者必须放在同一目录（helper 默认从自己旁边加载 DLL）。

## 运行

```powershell
# 常驻：管道服务 + 监控 ShellHost，首次和重启后自动注入
target\release\brighttray-helper.exe serve

# 手动注入当前 ShellHost
target\release\brighttray-helper.exe inject

# 以 suspended 方式重启 ShellHost 并抢在初始化前注入（fallback）
target\release\brighttray-helper.exe inject --early

# 只读诊断：查询 DisplayBrightness 和 QuickAction 对象，不 patch
target\release\brighttray-helper.exe probe
```

`serve` 默认自动注入；设置 `BRIGHTTRAY_NO_HOOK=1` 可只启动管道服务。

## 管道协议

`\\.\pipe\BrightTray`，多客户端，一行一条命令：

```
list                 -> index\tcurrent\tmax\tdescription，最后空行
get <index>          -> "current max" 或 "err"
set <index> <0-100>  -> 无回复，合并连续设置
getall               -> 第一个 DDC/CI 显示器的百分比，或 "err"
setall <0-100>       -> 无回复，应用到所有 DDC/CI 显示器
```

## 已验证环境

- Windows 11 Insider build 26300，Lenovo Y27q-30（DDC/CI）。
- `SettingsHandlers_PCDisplay.dll` / `QuickActionsDataModel.dll` 版本随系统更新变化；hook 安装时会
  校验 vtable、接口槽位和 `get_Id`，不符就放弃，不会弄崩 ShellHost。
