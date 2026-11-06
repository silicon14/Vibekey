# vibekey-win 实现规范（v1）

本文件是 crates/vibekey-win 的实现约定。代码质量与 Win7 兼容性按此验收。

## 总体要求

- 只使用 windows-sys 0.59（workspace 已引入）和 std，不引入 winapi、nwg、egui、tauri 等其它 GUI/系统依赖。
- 所有字符串处理用 UTF-16 W 系列 API。
- 程序是 GUI 子系统（#![windows_subsystem = "windows"]），无控制台窗口。
- release 配置 panic=abort：不能出现跨 FFI 的 panic；所有可能失败的操作都要记录日志并给出用户提示。
- 只用 Windows 7 已存在的 API。禁止：SetProcessDpiAwarenessContext、GetDpiForWindow、GetSystemTimePreciseAsFileTime、
  SetThreadDescription、LoadLibraryEx 的 LOAD_LIBRARY_SEARCH_*、WebView2、UCRT 依赖。DPI 用 SetProcessDPIAware。
- 代码组织：每个 unsafe 块写 // SAFETY: 说明。不要用 unwrap() 处理用户输入/系统调用返回值。

## 模块与职责

### main.rs

- 设 DPI aware（SetProcessDPIAware）。
- 单实例：CreateMutexW("Local\\Vibekey.SingleInstance")；已存在则直接退出。
- 初始化日志，加载配置，创建隐藏主窗口，跑消息循环。
- 退出时清理托盘图标、卸载钩子、停止工作线程。

### app.rs

App 状态：配置路径、Config、Keymap、SecretStore、主窗口 HWND、托盘、钩子线程句柄、
同步线程句柄、注入工作线程、定时器 id、弹窗/设置窗口状态、最近错误。

主窗口：隐藏的 WS_POPUP 顶层窗口，类名 "VibekeyMainWnd"，WS_EX_TOOLWINDOW，不进任务栏。
消息：

- WM_CREATE：加载配置（失败弹 MessageBox 并继续用默认配置，不覆盖坏文件）、Keymap::build（失败提示并禁用触发）、
  添加托盘图标、启动钩子线程、启动同步线程、SetTimer 轮询配置文件 mtime。
- WM_APP_ACTION（自定义，wparam=动作码，lparam=片段索引）：
  - LIST：打开片段列表（立即，不等待 Alt 松开）。
  - CONFIG：打开配置菜单（立即）。
  - SNIPPET：把 (片段索引, 触发时的前台 HWND) 交给注入线程，注入线程等 Alt 松开后执行。
- WM_APP_INJECT_DONE：注入成功/失败；失败时托盘气泡提示（节流）。
- WM_APP_TRAY（NOTIFYICONDATA.uCallbackMessage）：WM_RBUTTONUP/WM_CONTEXTMENU 弹配置菜单；
  WM_LBUTTONUP 打开片段列表；WM_LBUTTONDBLCLK 打开设置。
- WM_APP_RELOAD：重新读配置（同步拉取后、用户点“重新加载”、文件变化）。坏配置保留旧配置并气泡提示。
- WM_APP_SYNC_NOW：请求同步线程立即同步。
- TaskbarCreated（RegisterWindowMessageW）：Explorer 重启后重新添加托盘图标。
- WM_TIMER：配置文件 mtime 变化检测；延迟恢复剪贴板；Alt 松开轮询（如果注入线程用主线程实现）。
- WM_QUERYENDSESSION / WM_ENDSESSION：移除托盘图标、保存状态后退出。
- WM_DESTROY：PostQuitMessage。

### hook.rs（低级键盘钩子）

独立线程：SetWindowsHookExW(WH_KEYBOARD_LL, proc, GetModuleHandleW(null), 0)，然后 GetMessageW 循环；
主线程用 PostThreadMessageW(WM_QUIT) 结束它。安装结果通过 channel 或事件通知主线程。

回调规则（必须快速返回，不分配大对象）：

- 忽略 LLKHF_INJECTED 事件。
- 记录 Alt 按下时刻（VK_LMENU/VK_RMENU/VK_MENU 的 WM_KEYDOWN/WM_SYSKEYDOWN），用于 hold 模式。
- 只在 keydown 处理；按住不放的自动重复（同一个 vk 在 < 80ms 内重复）忽略。
- 通过 GetAsyncKeyState / 自维护状态计算 Ctrl/Shift/Win 是否按下，组合键必须和绑定完全一致才触发。
- 当前台窗口属于本进程时直接 CallNextHookEx。
- 前台进程 exe 名在 config.ignore_apps 里（不区分大小写）时直接透传。
- VK 到 core::keymap::Key 的映射：A-Z、0-9、VK_OEM_3(反引号)、VK_OEM_MINUS(-)、VK_OEM_PLUS(=)、
  VK_BACK、VK_TAB、VK_RETURN、VK_ESCAPE、VK_SPACE、VK_DELETE、VK_INSERT、方向键、Home/End/PgUp/PgDn、F1-F24。
- 命中 Keymap::resolve 时：PostMessageW(主窗口, WM_APP_ACTION, 动作码, 索引)，返回 1 吞键。
  SNIPPET 动作还要附带触发瞬间的 GetForegroundWindow()，把 HWND 塞进 lparam 或在主线程重新获取
  （前台窗口在几十毫秒内一般不变；为稳妥用 GetForegroundWindow 在钩子线程里取一次并打包）。
- 其它情况 CallNextHookEx，返回其返回值。
- 卸载：UnhookWindowsHookExW；钩子线程退出前确保窗口消息循环结束。

注意：低层钩子超时会被系统移除，回调里禁止 Sleep、文件 IO、网络。

### inject.rs（输入工作线程）

独立线程 + mpsc<InjectJob>。主线程只投递任务。任务字段：片段文本（已展开）、send_enter、目标 HWND、input_method、
restore_clipboard。

流程：

1. 等待物理 Alt 松开（GetAsyncKeyState(VK_MENU) 高位为 0），最多 2 秒；超时则继续但记录日志。
2. 若目标 HWND 仍有效且不等于当前前台窗口，尝试 SetForegroundWindow（失败则继续用当前焦点，记录日志）。
3. 检查目标进程权限：OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) 失败且 GetLastError=5 时视为管理员窗口，
   报告“无法向管理员权限窗口输入”，放弃。
4. paste 模式：
   - 先用 EnumClipboardFormats 检查：纯文本才保存 CF_UNICODETEXT 并粘贴；文件/图片/富文本时不要覆盖剪贴板，
     改为逐字符输入（实现：clipboard::inspect() → ClipboardContent::Rich 时回退 type 模式）。
   - OpenClipboard 重试 10 次、每次 10ms；EmptyClipboard；GlobalAlloc(GMEM_MOVEABLE) 写入 UTF-16（含结尾 0）；
     SetClipboardData(CF_UNICODETEXT)。
   - Sleep 50ms 后 SendInput Ctrl+V（4 个事件：Ctrl down, V down, V up, Ctrl up，V 用 KEYEVENTF_UNICODE 不需要；
     用虚拟键 VK_CONTROL/VK_V）。
   - 若 send_enter：Sleep 30ms 后发送 VK_RETURN 按下/抬起。
   - 250ms 后恢复剪贴板文本（restore_clipboard 为 false 时跳过）。恢复必须在注入工作线程内串行执行，
     避免主线程 TIMER 与后续注入任务竞争导致恢复成片段文本；原始剪贴板为空时恢复为空。
   - 任何一步失败 → 回退到 type 模式。
5. type 模式：对 UTF-16 code unit 逐个 SendInput(KEYEVENTF_UNICODE)，每 2ms 一个，每 200 个字符让出 5ms。
   文本中的 \n 转换为 VK_RETURN 按下/抬起（KEYEVENTF_UNICODE 的 \n 在部分程序里无效）。
6. 结果 PostMessage 主窗口 WM_APP_INJECT_DONE（wparam 0 成功 / 1 失败），失败原因写到 App 的 last_error。

所有 SendInput 前都要确认物理 Ctrl/Shift/Win 没被按住（等待 100ms，超时则强发）。

### ui/tray.rs

- Shell_NotifyIconW(NIM_ADD)：uID=1，uFlags=NIF_ICON|NIF_MESSAGE|NIF_TIP，hWnd=主窗口，uCallbackMessage=WM_APP_TRAY，
  szTip = "Vibekey - Alt+反引号打开列表"。
- 图标：include_bytes!("../assets/icon.ico")，解析 ICO 头（ICONDIR/ICONDIRENTRY），对 32x32 条目调用
  CreateIconFromResourceEx(数据指针, 大小, TRUE, 0x00030000, 0, 0, LR_DEFAULTCOLOR)，失败则 LoadIconW(IDI_APPLICATION)。
  注意 ICO 里的图像是 BMP（非 PNG），CreateIconFromResourceEx 可直接用。
- 气泡：NIF_INFO，NIIF_INFO/NIIF_WARNING，标题 "Vibekey"。同一类错误 10 分钟内不重复提示。
- TaskbarCreated 后重新添加。
- 退出时 NIM_DELETE。

### ui/menu.rs（配置菜单）

TrackPopupMenu（右键托盘或 Alt+Backspace）菜单项：

1. 片段列表...
2. 编辑配置文件（notepad.exe 打开 config.toml）
3. 重新加载配置
4. 立即同步（未启用同步时禁用；同步中禁用）
5. WebDAV 设置...
6. 开机启动（勾选状态来自 Config.start_with_windows，点击写注册表 + 保存配置）
7. 打开日志文件
8. 关于（版本号、快捷键说明、配置路径）
9. 退出

实现要点：SetForegroundWindow(主窗口) → TrackPopupMenu(TPM_RETURNCMD|TPM_RIGHTBUTTON) →
PostMessageW(主窗口, WM_NULL, 0, 0)。坐标用 GetCursorPos（托盘）或光标插入符位置（Alt+Backspace 用
GetGUIThreadInfo 的 rcCaret → ClientToScreen，失败回退 GetCursorPos）。

### ui/popup.rs（片段列表）

- 自绘窗口类 "VibekeyPopup"：WS_POPUP|WS_BORDER，WS_EX_TOPMOST|WS_EX_TOOLWINDOW，子控件 ListBox（WS_CHILD|WS_VISIBLE|
  WS_VSCROLL|LBS_NOTIFY|LBS_NOINTEGRALHEIGHT）。
- 每条目："1. 名称 — 首行预览"（预览截断到约 40 字符，换行符替换为空格）；无快捷键的条目显示 "-."。
- 尺寸：宽约 520 逻辑像素（按 DPI 缩放），高 min(条目*22+8, 420)；位置优先插入符：
  GetGUIThreadInfo(GetWindowThreadProcessId(GetForegroundWindow())) → gui.rcCaret → ClientToScreen；
  失败用 GetCursorPos；再向屏幕内收（MonitorFromPoint / GetSystemMetrics(SM_CXSCREEN/SM_CYSCREEN)）。
- 显示后 SetForegroundWindow + SetFocus(ListBox) + 选中第一项。
- 交互：Enter / 双击 / 单击条目（LBN_SELCHANGE 后立即执行）→ 隐藏弹窗并把片段索引发给注入线程；Esc → 关闭；
  WM_KILLFOCUS / WM_ACTIVATE(WA_INACTIVE) → 关闭；点击空白 → 关闭。
  单击立即执行和方向键选择会冲突，所以只在 LB_GETCURSEL 对应项被真正点击时执行（用 ListBox 的 LBN_SELCHANGE 无法区分，
  建议子类化 ListBox 处理 WM_LBUTTONUP，或使用 LBN_DBLCLK + Enter，二选一并在代码注释说明）。
- 弹窗打开期间钩子透传，不触发片段。

### ui/settings.rs（设置窗口）

普通顶层窗口（WS_OVERLAPPEDWINDOW 去掉最大化），标题 "Vibekey 设置"，控件用 EDIT/BUTTON/COMBOBOX 程序化创建，
字体 DEFAULT_GUI_FONT，支持 Tab 导航（消息循环里对设置窗口调用 IsDialogMessageW）。

控件：

- 同步：启用同步 checkbox；WebDAV URL edit；用户名 edit；密码 edit（ES_PASSWORD）；间隔分钟 edit；
  保存后同步 checkbox；冲突时备份远端 checkbox。
- 行为：输入方式 combo（粘贴 / 直接输入）；触发方式 combo（立即 / 按住 Alt）；开机启动 checkbox。
- 按钮：保存（IDOK）、取消（IDCANCEL）、测试连接。
- 测试连接：用当前 URL/用户名/密码做一次 GET（超时 10 秒），MessageBox 显示结果；必须能取消/不卡死（可用工作线程）。

保存流程：构造新 Config（密码单独走 SecretStore::store_password）→ Config::save → 更新开机启动注册表 →
通知主窗口 WM_APP_RELOAD → 触发一次同步（若启用）。保存失败 MessageBox 错误。
取消/关闭不落盘。

### secret.rs

实现 vibekey_core::SecretStore：DPAPI CryptProtectData / CryptUnprotectData（可选熵 b"Vibekey"），
密文 base64 后写入 %APPDATA%\Vibekey\secret.bin（原子写）。读取失败返回 SecretError::Unavailable/Decrypt。
日志里绝不能出现明文密码。

### autostart.rs

HKCU\Software\Microsoft\Windows\CurrentVersion\Run，值名 "Vibekey"，数据为带引号的 exe 全路径
（GetModuleFileNameW 得到）。提供 set_enabled(bool) -> Result<(), String> 与 is_enabled()。

### sync.rs（同步线程）

线程 + mpsc<Command>：SyncNow、ConfigChanged、Shutdown。循环用 recv_timeout(interval)；
间隔来自 Config.sync.interval_minutes（0 表示不定时，只手动/保存时同步）。

每次同步：

- 读取当前 Config 快照（由主线程通过 ConfigChanged 更新），未启用或 URL 为空则跳过。
- 密码从 SecretStore 读取；缺失则记录日志并气泡提示（节流），跳过。
- UreqWebDav::new(username, password, timeout) → SyncEngine::new(dav, url, config_path, backup_on_conflict)
  → state = SyncState::load(SyncState::path_for(config_path)) → engine.sync(&mut state, timestamp)
  → state.save。
- 时间戳用 chrono::Local::now().format("%Y%m%d-%H%M%S")。
- Pulled → PostMessage WM_APP_RELOAD；错误 → 记录日志 + 气泡（节流），state.record_error 并保存。
- 手动同步完成后通过主窗口气泡提示结果（成功/冲突/失败）。

### log.rs

文件日志 %APPDATA%\Vibekey\vibekey.log，超过 1MB 轮转为 vibekey.log.1；行格式 "时间 级别 模块: 消息"；
实现 log::Log + set_logger（只设置一次）。初始化失败时降级为 OutputDebugStringW（可选）。级别可用环境变量
VIBEKEY_LOG=debug 控制。

## 与 core 的接口

允许使用（已实现，勿改）：Config/Config::load_or_create/save/validate/snippet_for_key/visible_snippets、
Keymap::build/resolve/list_hotkey/config_hotkey、Binding、Hotkey/Key/Modifiers、expand/ExpandContext、
SyncEngine/WebDav/UreqWebDav/SyncState/SyncError/SyncOutcome、SecretStore/SecretError。

需要新增 core API 时不要直接改 core：把需求发给 Lead（message），Lead 统一修改。

## 构建与自检

    . scripts/dev-env.sh
    cargo +nightly build --release -Z build-std=std,panic_abort --target x86_64-win7-windows-gnu -p vibekey
    cargo +nightly build --release -Z build-std=std,panic_abort --target i686-win7-windows-gnu -p vibekey
    cargo test -p vibekey-core

自检 PE：

    llvm-readobj --file-headers target/x86_64-win7-windows-gnu/release/vibekey.exe | grep -i subsystem
    # 期望 MajorSubsystemVersion=6 MinorSubsystemVersion=1 或 0
    llvm-readobj --coff-imports target/x86_64-win7-windows-gnu/release/vibekey.exe | grep 'Name:'
    # 只允许 KERNEL32 USER32 GDI32 SHELL32 ADVAPI32 msvcrt WS2_32 bcrypt ntdll USERENV OLE32 等 Win7 自带 DLL

## 验收标准

1. 两个 target 交叉编译通过，0 error；不出现新增 clippy 严重警告（允许 unsafe 相关）。
2. 主窗口消息循环、托盘图标、钩子、剪贴板注入、列表弹窗、配置菜单、设置窗口、同步线程、DPAPI、开机启动全部可用
   （按代码审查判断；无法在 Linux 运行）。
3. 配置文件坏掉时程序不崩溃，提示并保留旧配置。
4. 所有用户可见文案为中文；代码注释可用英文。
5. 不修改 crates/vibekey-core 与 scripts/、docs/、.github/ 下文件。文件范围：crates/vibekey-win/**（assets/icon.ico 已由 Lead 提供，勿覆盖）。
6. 完成后在自己的最后一条消息里列：已实现文件清单、未实现/待办、编译命令与结果、风险点。
