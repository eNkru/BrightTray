# 控制中心亮度条 Hook 调研

环境：Windows 11 build 26300（Insider），台式机（MSI MS-7E01），单屏 Lenovo Y27q-30（DDC/CI 可用）。
调研日期：2026-11-12。所有 RVA 均针对本机当前版本的 `SettingsHandlers_PCDisplay.dll`，仅供参考，方案本身不依赖它们。

## 1. 亮度条在哪个进程、由谁实现

| 层 | 模块 | 说明 |
|---|---|---|
| UI | `ShellHost.exe` → `ControlCenter.dll` | 24H2 之后的快速设置（Win+A）宿主；`ShellExperienceHost` 里的 `Windows.UI.QuickActions.dll` 不是当前在用的那份 |
| 磁贴列表 | `QuickActionsDataModel.dll` | 从 `HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\ActionCenter\Quick Actions\All` 读取磁贴 |
| 亮度磁贴 | `SystemSettings_Display_Brightness`（FriendlyName `Microsoft.QuickAction.Brightness`） | 本质是一个"设置页设置项" |
| 设置项实现 | `C:\Windows\System32\SettingsHandlers_PCDisplay.dll` | 由 `HKLM\SOFTWARE\Microsoft\SystemSettings\SettingId\SystemSettings_Display_Brightness\DllPath` 指定；ShellHost 已加载 |
| 类 | `SystemSettings::DataModel::DisplayBrightness`（实现 `ISettingItem` / `ISettingItem2` / `ISettingItemTelemetry`） | |
| 底层 | `Windows.Internal.Graphics.Display.DisplayEnhancementManagement` → DisplayEnhancementService | 真正调亮度 |

导出函数（稳定 ABI）：`HRESULT GetSetting(HSTRING id, ISettingItem** out)`，内部直接跳到 `GetSettingImpl`。

## 2. ISettingItem vtable（从 PDB 公共符号确认）

| slot | 方法 | DisplayBrightness 实现 |
|---|---|---|
| 0–5 | IUnknown / IInspectable | |
| 6 | `get_Id(HSTRING*)` | 基类 |
| 7 | `get_Type(SettingType*)` | 基类 |
| 8 | `get_IsSetByGroupPolicy(BYTE*)` | 基类 |
| **9** | `get_IsEnabled(BYTE*)` | `DisplayBrightness` |
| **10** | `get_IsApplicable(BYTE*)` | `DisplayBrightness` —— **决定亮度条是否出现** |
| 11 | `get_Description(HSTRING*)` | |
| 12 | `get_IsUpdating(BYTE*)` | 基类 |
| **13** | `GetValue(HSTRING name, IInspectable** value)` | `DisplayBrightness` |
| **14** | `SetValue(HSTRING name, IInspectable* value)` | `DisplayBrightness` —— **拖动亮度条走这里** |
| 15/16 | `GetProperty` / `SetProperty` | 基类 |
| 17 | `Invoke(ICoreWindow*, Rect)` | 基类 |
| **18/19** | `add_SettingChanged` / `remove_SettingChanged`（`ITypedEventHandler<IInspectable*, HSTRING>`） | 基类 —— UI 靠它刷新 |

`GetValue` 支持的属性名：`Value`（Int32 百分比）、`MinValue`、`MaxValue`（Int32）、`Values`、`QuickActionIsActive`（Boolean）、`QuickActionStatus`（String，格式化后的"xx%"）。
`SetValue("Value", Int32)` → `DisplaySettingsManager::SetBrightnessPercent`。

## 3. 为什么台式机上没有亮度条

`get_IsApplicable` = `!IsDisabledByPolicy() && IsBrightnessSupported()`。
`IsBrightnessSupported()`：

1. 若 WIL 特性 `Feature_ExternalBrightness` 开启，枚举过滤器用 `0x1a`（含外接屏），否则 `0x0a`（只看内屏）；
2. 对每个目标问 `IDisplayEnhancementManagement::IsBrightnessCapable`，有一个为真就支持。

本机只有外接屏，又没开该特性 → 返回 false → 磁贴被隐藏。`HandleNotification` 收到通知时会对 `IsApplicable` / `IsEnabled` / `Values` / `Value` / `QuickActionStatus` 触发 `SettingChanged`（内部函数 `CSettingBase::OnValueChanged`）。

## 4. 顺带发现：Windows 自带的外接屏 DDC/CI 亮度（未开放）

- `SettingsHandlers_PCDisplay.dll` 里有 `Feature_ExternalBrightness`（ID **12759424**）和 `Feature_ExternalBrightness_UIErrorReporting`（ID **54920936**），还有 "NoDdcciSupport"、"NoI2CSupport"、"DisabledByOem" 之类的提示文案。
- DisplayEnhancementService 里有 `nitsbrightnessadapterexternalmonitor.cpp`。
- 本机查询结果：12759424 被**显式禁用，优先级 9**（高于 ViVeTool 默认的 User=8），需要更高优先级的覆盖才能打开，且要管理员权限；服务端可能还有自己的特性开关，并依赖显卡驱动的 I2C 支持。没有实测。
- 若将来系统正式放出该功能，原生亮度条就能直接用 DDC/CI 调外接屏，本 Hook 可以退役。

## 5. 注入可行性

- ShellHost：当前用户、Medium 完整性，父进程 `sihost.exe`，非 PPL。
- 缓解措施：无 MicrosoftSignedOnly、无 BlockDynamicCode、无远程/低完整性映像加载限制；仅开启普通 CFG（非严格）。
  → 同用户 Medium 进程可以 `CreateRemoteThread(LoadLibraryW)` 注入未签名 DLL。未开启 CFG 的 DLL 整个模块都被视为合法间接调用目标，vtable 替换函数不会被 CFG 拦。
- `SettingsHandlers_PCDisplay.dll` 几乎去掉了 RTTI（没有 `DisplayBrightness` 的类型描述符），不能靠 RTTI 定位 vtable。

## 6. 推荐 Hook 方案（不依赖任何 RVA / 符号）

1. 注入 `brighttray_hook.dll` 到 `ShellHost.exe`。
2. 在 ShellHost 内 `GetModuleHandle("SettingsHandlers_PCDisplay.dll")` → `GetProcAddress("GetSetting")`，用 `SystemSettings_Display_Brightness` 拿到一个实例；它的第一个指针就是 `DisplayBrightness` 的 `ISettingItem` vtable（位于该 DLL 的 .rdata，所有实例共享）。校验 vtable 地址在模块映像范围内。
3. `VirtualProtect` 后替换 slot：
   - 9 / 10 → 恒返回 TRUE（亮度条出现、可用）
   - 13 → `Value` / `QuickActionStatus` 返回 DDC 的当前亮度，`MinValue` / `MaxValue` 给 0/100，其余转调原函数
   - 14 → `Value` 时取 Int32，发给 BrightTray helper（命名管道 `setall <p>`），返回 S_OK；其余转调原函数
   - 18 / 19 → 先调原函数，同时把 handler 记下来（AddRef），以后外部改了亮度时自己调用 `handler->Invoke(sender, "Value")` 刷新滑块
4. 已经注册过的 handler 抓不到，所以注入后需要让 ShellHost 重建快速设置（重启 ShellHost 并尽早注入，或者确认 UI 每次打开浮窗都会重新查询）——这一点要在原型里实测。

依赖面：导出函数 `GetSetting`、ISettingItem 的槽位布局、属性名字符串。这三者从 Win10 到现在都没变过，比按 RVA 打补丁稳得多。

### 风险 / 待验证

- ControlCenter 是否还有额外的显示条件（只在启动时查询一次 IsApplicable？还要看 DisplayEnhancement？）——原型实测。
- 系统更新会替换 DLL，但只要上面三点不变就不用改代码；hook 安装时做 vtable 地址与 `get_Id` 返回值校验，不符就放弃，不会弄崩 ShellHost。
- ShellHost 崩溃会被 sihost 拉起（待确认），注入代码必须足够保守。
- 杀软可能把远程线程注入当可疑行为。
- 管道服务器目前单连接，托盘程序和 hook DLL 同时连会冲突，需要改成多实例。

## 7. 实测补充（2026-11-12，原型已完成）

原方案的第 2–3 步（只 patch `SettingsHandlers_PCDisplay.dll` 的 `ISettingItem`）在本机**不足以让快速设置的亮度条出现**。实测结论：

- 打开快速设置时，`ShellHost.exe` 根本没有向 `DisplayBrightness` 的 `ISettingItem` 查询 `IsApplicable` / `GetValue`，日志里没有任何调用。
- 真正的 UI 绑定层是 `C:\Windows\System32\QuickActionsDataModel.dll` 里的 `QuickSetting`（实现 `Windows.Internal.QuickActions.IQuickAction`）。它包装 `ISettingItem`，并缓存 `IsApplicable` / `IsEnabled` / 状态。快速设置的 "Sliders" 分组来自用户布局：
  `HKCU\Control Panel\Quick Actions\Control Center\UserLayoutPaginated`，其中已经包含
  `{"FriendlyName":"Microsoft.QuickAction.Brightness"}`，所以不需要改注册表。
- `QuickSetting` 的 vtable 是**所有快速设置共用的**（WiFi、蓝牙、夜间模式等都是同一个 vtable）。只对亮度返回 TRUE 而不区分实例，会把其他磁贴的 `GetValue` / 状态也改成亮度值，导致磁贴文字/状态错乱。必须先用 `get_Id` 判断实例。

### QuickSetting / IQuickAction vtable（本机 QuickActionsDataModel.dll）

| slot | 方法 | 处理 |
|---|---|---|
| 7 | `get_Id(HSTRING*)` | 校验 `SystemSettings_Display_Brightness` |
| 8 | `get_FriendlyName(HSTRING*)` | 校验 `Microsoft.QuickAction.Brightness` |
| **9** | `get_IsApplicable(BYTE*)` | 亮度实例恒 TRUE，其他转调原函数 |
| **10** | `get_IsEnabled(BYTE*)` | 亮度实例恒 TRUE，其他转调原函数 |
| **12** | `get_QuickActionStatus(HSTRING*)` | 亮度返回当前 DDC 百分比，其他转调 |
| **14** | `get_QuickActionIsActive(BYTE*)` | 亮度恒 TRUE，其他转调 |
| **19/20** | `add/remove_QuickActionPropertyChanged` | 只登记亮度实例的 UI handler |
| **21** | `GetValue(HSTRING, IInspectable**)` | `Value` / `MinValue` / `MaxValue` / `QuickActionStatus` / `QuickActionIsActive` |
| **22** | `SetValue(HSTRING, IInspectable*)` | `Value` 发给 helper `setall <p>`，其他转调 |

实现里通过 `QuickActionsDataModel.dll` 的导出 `GetQuickActionsProvider` 拿到 provider，
再调用 `IQuickActionsProvider::GetQuickActionByFriendlyName`（provider vtable slot 13）取得
亮度 `QuickSetting` 实例；校验 `get_Id` 后 patch 共享 vtable。这样不依赖 `QuickSetting`
vtable 的 RVA。`get_Id` 是 `WindowsDuplicateString` 的包装，返回的 HSTRING 由调用方释放。

### 已验证行为

- `probe`：`GetSetting` 成功；原 `IsApplicable=0`、`IsEnabled=0`、`Value=0`、`MaxValue=100`。
- `probe`：`GetQuickActionsProvider` + `GetQuickActionByFriendlyName("Microsoft.QuickAction.Brightness")`
  成功，返回 `QuickSetting`。
- 注入后打开快速设置：日志出现 `QuickIsApplicable`、`QuickIsEnabled`、`GetValue("MaxValue"/"MinValue"/"Value")`，
  亮度条出现（与音量滑块并排）。
- 拖动亮度条：日志出现 `QuickSetValue("Value")`，外接 Lenovo Y27q-30 的实际亮度改变；
  后台 watcher 随后看到 DDC 变化并回调已登记的 `QuickActionPropertyChanged` handler。
- WiFi/蓝牙磁贴文字恢复正常（按 `get_Id` 过滤其他 QuickSetting 实例后）。
- 拖动结束后 1.2 秒内 watcher 忽略 DDC 回读，避免显示器响应慢时把滑块拉回旧值。

### 当前实现与使用

```
cargo build --release
target\release\brighttray-helper.exe serve      # 管道服务器 + 监视 ShellHost 并自动注入
target\release\brighttray-helper.exe inject     # 手动注入当前 ShellHost
target\release\brighttray-helper.exe inject --early [dll]
target\release\brighttray-helper.exe probe      # 只读验证，不会 patch
```

- `serve` 默认会在 ShellHost 重启后自动补注入；`BRIGHTTRAY_NO_HOOK=1` 可关闭。
- 管道协议新增 `getall` / `setall <p>`，并把服务器改为**多实例、每客户端一个线程**，
  托盘程序和 hook DLL 可以同时连接。
- `inject --early` 会以 suspended 方式启动 ShellHost、注入并等待 hook 的 ready 事件后再 resume；
  在本机实测中最终工作路径是**普通注入 + QuickSetting vtable patch**，不依赖 early 注入。
- hook DLL 会 pin 自身（`GetModuleHandleExW(...PIN...)`），避免 vtable 还引用着代码时被卸载。
- 目前只把 hook 注入 `ShellHost.exe`；`SettingsHandlers_PCDisplay.dll` 的 `ISettingItem` patch 保留，
  作为 Settings 页/未来系统组件的兼容层。`get_IsApplicable` 的检查、`get_Id` 校验和 vtable
  地址校验失败时都不会 patch，不会弄崩 ShellHost。

### 剩余风险

- 系统更新替换 `QuickActionsDataModel.dll` 后，`IQuickAction` 槽位布局如果变化，安装校验会失败并放弃；
  需要按新 PDB 更新槽位常量。
- 快速设置的 UI 内部是否还有别的缓存路径尚未覆盖；目前实测的打开/拖动/外部变更都正常。
- 杀软仍可能把远程线程注入报为可疑行为。

