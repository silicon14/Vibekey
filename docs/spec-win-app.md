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

### build.rs / main.rs

- build.rs 将 assets/icon.ico 和中文 FileDescription、ProductName、Cargo 版本写入生成的 RC，再用 embed-resource 链接到 exe。
  资源只用于装饰，交叉工具链缺少资源编译器时不得阻断功能程序构建；Cargo.toml / icon 变化需触发重建。
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
  - HISTORY：打开内存文本剪贴板历史。
  - CONFIG：通过 WM_APP_SHOW_MENU 排队打开配置菜单。
  - SNIPPET：把 (片段索引, 触发时的前台 HWND) 交给注入线程，注入线程等 Alt 松开后执行。
- WM_CLIPBOARDUPDATE：用 HistoryControl 的互斥/序列号协议读取外部 Unicode 文本；忽略内部粘贴和恢复。
- WM_APP_SHOW_MENU：取出排队的菜单坐标，TrackPopupMenu 前后分别短借用 App，禁止跨模态菜单保留 &mut App。
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

独立线程 + mpsc<InjectJob>。主线程只投递任务。任务字段：template、expand_template、send_enter、目标 HWND、
input_method、restore_clipboard。片段在工作线程展开；历史 expand_template=false，必须原样粘贴。所有剪贴板写入和恢复
都经 HistoryControl::internal_write，避免历史监听器采集内部操作。

流程：

1. 等待物理 Alt 松开（GetAsyncKeyState(VK_MENU) 高位为 0），最多 2 秒；超时则继续但记录日志。
2. 若目标 HWND 仍有效且不等于当前前台窗口，尝试 SetForegroundWindow（失败则继续用当前焦点，记录日志）。
3. 检查目标进程权限：OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) 失败且 GetLastError=5 时视为管理员窗口，
   报告“无法向管理员权限窗口输入”，放弃。
4. 菜单模式处理：Alt 松开后固定本次目标 HWND/UI thread，用 GUI_INMENUMODE/GUI_POPUPMENUMODE 做有界轮询（150ms 沉降窗口），
   检测到菜单才发送一次 Esc，再从实际发送时起最多 250ms 确认退出，避免 Explorer 的菜单尚未进入就提前放行 Ctrl+V。
   等待期间前台窗口变化必须取消本次输入，不得关闭其它窗口菜单或注入错误目标。不得无条件 Esc，以免取消地址栏编辑。
5. paste 模式：
   - `clipboard::snapshot()` 完整复制当前剪贴板：EnumClipboardFormats 枚举后逐个保存。文本/PNG/DIB/DIBV5/HDROP/
     HTML/RTF/OLE 数据等按 HGLOBAL 复制；CF_BITMAP 用 GetObjectW 估算大小并用 CopyImage 复制。
     无法安全复制的格式（CF_METAFILEPICT、CF_PALETTE、CF_ENHMETAFILE、CF_OWNERDISPLAY、DSP/私有/GDI 格式）、
     快照超过 128 MB 或打开剪贴板失败时返回错误，交给 type 模式并保持剪贴板不变。
   - OpenClipboard 重试 10 次、每次 10ms；EmptyClipboard；GlobalAlloc(GMEM_MOVEABLE) 写入 UTF-16（含结尾 0）；
     SetClipboardData(CF_UNICODETEXT)。
   - Sleep 50ms 后 SendInput Ctrl+V（4 个事件：Ctrl down, V down, V up, Ctrl up；用 VK_CONTROL/VK_V）。
   - 若 send_enter：Sleep 30ms 后发送 VK_RETURN 按下/抬起。
   - 250ms 后 `ClipboardSnapshot::restore()` 重新 EmptyClipboard 并逐个 SetClipboardData 恢复全部格式
     （restore_clipboard 为 false 时跳过快照与恢复）。恢复必须在注入工作线程内串行执行，避免主线程 TIMER
     与后续注入任务竞争导致恢复成片段文本；原始剪贴板为空时恢复为空。
   - Ctrl+V 或回车失败时也要恢复快照，不能让片段留在剪贴板；恢复失败只记日志，不把成功粘贴改为失败。
   - 快照失败或任何一步失败 → 回退到 type 模式。
6. type 模式：先累积 INPUT 事件，每 512 个事件调用一次 SendInput(KEYEVENTF_UNICODE)，块与块之间 yield，
   不再逐字符 sleep；文本中的 \n 转换为 VK_RETURN 按下/抬起（KEYEVENTF_UNICODE 的 \n 在部分程序里无效）。
7. 结果 PostMessage 主窗口 WM_APP_INJECT_DONE（wparam 0 成功 / 1 失败），失败原因写到 App 的 last_error。

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

1. 片段列表… / 剪贴板历史… / 新建片段… / 设置与片段管理…
2. 清空剪贴板历史（无历史时禁用）
3. 编辑 TOML 配置（notepad.exe 打开 config.toml）
4. 重新加载配置
5. 立即同步（未启用同步时禁用；同步中禁用）
6. 开机启动（勾选状态来自 Config.start_with_windows，点击写注册表 + 保存配置）
7. 打开日志文件
8. 关于（版本号、快捷键说明、配置路径）
9. 退出

实现要点：SetForegroundWindow(主窗口) → TrackPopupMenu(TPM_RETURNCMD|TPM_RIGHTBUTTON) →
PostMessageW(主窗口, WM_NULL, 0, 0)。坐标用 GetCursorPos（托盘）或光标插入符位置（Alt+Backspace 用
GetGUIThreadInfo 的 rcCaret → ClientToScreen，失败回退 GetCursorPos）。

### ui/popup.rs（片段与剪贴板历史选择器）

- 自绘窗口类 "VibekeyPopup"：WS_POPUP，WS_EX_TOPMOST|WS_EX_TOOLWINDOW|WS_EX_LAYERED；共享 ui::theme 的浅色配色和中文字体。
- SetLayeredWindowAttributes(LWA_ALPHA) 使用 242/255（约 95%）统一不透明度，不引入 GPU/UI 框架；失败时移除
  WS_EX_LAYERED 并回退为不透明。该 API 与 32 位 extended style 读写均兼容 Win7 x86/x64。
- 搜索 EDIT 与右侧 ☰ 菜单按钮同一行；历史模式另显示“清空”。无标题、结果数或底栏。LBS_OWNERDRAWFIXED ListBox
  每项只画一行：片段为 Alt 槽位徽标 + 名称 + 内容预览；历史为折叠后的 Unicode 文本。搜索使用未截断原文。
- 宽度约 460 逻辑像素、最大约 270 高，行高 34、最多 6 行。所有控件按 DPI 和 WM_SIZE 重排；宽高先按工作区限制。
  最终 SetWindowPos/SetFocus 后必须 LB_SETTOPINDEX=0，防止默认滚动把第一项裁掉。
- 鼠标命中同时校验 LB_ITEMFROMPOINT 与 LB_GETITEMRECT，过滤后点击末行以下的空白不得执行最近一项。
- 显示后 force_foreground + SetFocus，保留目标窗口记录与失焦关闭行为。☰ 仅计算坐标并调用 App::queue_menu，
  不得在持有 App 借用时直接 TrackPopupMenu。
- 交互：Enter / 单击 / 双击条目 → 隐藏弹窗并注入；Esc → 关闭。片段 F2 / 右键 →
  App::open_snippet_editor(Some(index))；新建和设置从 ☰ 菜单进入。设置窗仍按完整 Snippet 身份匹配，不能按旧位置猜测。
- 历史条目使用稳定 u64 ID，粘贴/Delete 前重新解析；Delete 仅删除选中的历史项，清空按钮删除全部。历史注入禁止模板展开。
- 弹窗打开期间钩子透传，不触发片段。

### history.rs（内存文本历史）

- ClipboardHistory 最新项在前，最多 50 条，空文本不记录，完全相同的文本去重置顶；u64 ID 单调分配且清空不复用。
- 不写磁盘、不记录明文日志、不参与同步，shutdown 清空。图片/文件/富文本 payload 不复制；仅记录 CF_UNICODETEXT。
- AddClipboardFormatListener/RemoveClipboardFormatListener 与 GetClipboardSequenceNumber 都是 Win7 可用 API。
- HistoryControl 的互斥门协调主线程捕获与 inject 写入。主线程 try_lock 后不睡眠读取，执行 sequence 前后复核；
  worker 写入持同一锁，并在 CloseClipboard 前返回本次写入的 sequence。只登记该独占所有权下取得的序列，
  不把操作结束后的外部最新序列误标为内部，避免自采集、TOCTOU 和外部历史漏采。

### ui/settings.rs（设置与片段管理）

普通可调整大小的 WS_OVERLAPPEDWINDOW 顶层窗口，标题 "Vibekey · 设置与片段管理"。共享 ui::theme 的字体、
画刷与自绘导航/列表，使用 EDIT/BUTTON/COMBOBOX/LISTBOX 程序化创建；消息循环继续用 IsDialogMessageW 支持 Tab 导航。
窗口分为三个页面；默认采用宽松双栏布局，较矮窗口自动切换紧凑布局，最小尺寸还会按工作区限制：

- 片段库：搜索、owner-drawn 列表、新建/复制/删除、上移/下移排序；右侧直接编辑名称、12 个 Alt 槽位（或不绑定）、多行正文、
  send_enter、enabled；模板变量 combo + 插入按钮用 EM_REPLACESEL 插入到当前光标。
- 输入与快捷键：paste/type、restore_clipboard、immediate/hold、hold_ms、start_with_windows、list/config/history
  三个全局热键、ignore_apps。所有 Config 行为字段都有图形化入口。
- 同步与备份：enabled、URL、用户名、WebDAV 登录密码、encrypt、独立配置加密密码、certificate_sha256、interval_minutes、timeout_secs、
  sync_on_change、backup_on_conflict、测试连接和分别清除两类密码。加密密码至少 8 个字符；测试发现加密远端时必须在后台线程
  实际解密验证。密码只走 SecretStore，清除操作在应用时执行，留空保留。

排序按 snippet_indices 当前可见顺序交换 Config.snippet 中的两个实际项，保留被移动项的新实际索引；过滤时同样只比较可见邻居。
首/末项按钮禁用，刷新后保持片段身份与 Alt 槽位，不能把排序当成位置热键变化。编辑先更新窗口内 Config 工作副本，取消/关闭有未保存确认，不直接写文件。应用/保存流程：
读取其它页面控件 → 校验数值范围、WebDAV URL 和 Keymap（含重复槽位）→ 检测 external mtime 变化并确认 →
保存/清除密码 → Config::save 原子写 → 更新开机启动 → PostMessage WM_APP_RELOAD → 按 sync_on_change 请求同步。
“应用”保持窗口打开，“保存并关闭”随后 DestroyWindow。保存失败显示中文 MessageBox 并保留编辑内容。

### secret.rs

实现 vibekey_core::SecretStore：DPAPI CryptProtectData / CryptUnprotectData。WebDAV 登录密码沿用 b"Vibekey" entropy
和 secret.bin；配置加密密码用独立 entropy 和 sync-encryption.bin，密文 base64 后原子写。后者在单个加密 JSON blob 内保存
current/previous：先尝试 current 解密，previous 只用于轮换，finish_password_rotation 必须比对本轮完整密码对再条件清理。
store/clear/finish 的完整读校验写由同一 DpapiSecretStore Mutex 串行化；pending previous 存在时禁止再次换成第三个密码，
防止 PUT 成功但状态未保存的崩溃边界丢失可用密码。
读取失败返回 SecretError::Unavailable/Decrypt。日志里绝不能出现明文密码。

### autostart.rs

HKCU\Software\Microsoft\Windows\CurrentVersion\Run，值名 "Vibekey"，数据为带引号的 exe 全路径
（GetModuleFileNameW 得到）。提供 set_enabled(bool) -> Result<(), String> 与 is_enabled()。

### sync.rs（同步线程）

线程 + mpsc<Command>：SyncNow、ConfigChanged、Shutdown。循环用 recv_timeout(interval)；
间隔来自 Config.sync.interval_minutes（0 表示不定时，只手动/保存时同步）。

每次同步：

- 读取当前 Config 快照（由主线程通过 ConfigChanged 更新），未启用或 URL 为空则跳过。
- 每轮开始一次性读取 WebDAV 密码及配置加密 current/previous，组成不可变 SyncProtection；encrypt=true 缺少 current 则跳过并提示。
  PBKDF2/加解密只在同步工作线程执行。匿名 WebDAV 用户名为空时允许登录密码为空。
- UreqWebDav::new_with_fingerprint(username, password, timeout, certificate_sha256) → SyncEngine::new(...).with_protection(...)
  → state = SyncState::load(...) → engine.sync。GET 先解密认证，所有 hash 都基于明文；所有 PUT（含备份/412/迁移）统一加密。
  marked + encrypt=false 仅在本机 state.remote_encrypted=Some(true) 时允许主动关闭迁移；None/Some(false) 返回 EncryptionRequired，禁止隐式明文覆盖。
- certificate_sha256 留空走默认 WebPKI；非空时用 DER SHA-256 固定单张自签证书，TLS1.2/1.3 签名仍用 ring/WebPkiSupportedAlgorithms 验证，禁止 accept-all。
  指纹模式同时 https_only(true)，禁止 HTTPS→HTTP 重定向；默认 HTTP 与普通 WebPKI 行为不变。
- engine 成功后必须先 state.save；保存失败保留 previous 并报告。保存成功后才按本轮 current/previous 条件提交轮换清理，
  防止设置页在网络等待期间写入更新密码对后被旧任务误删。
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
SyncEngine/WebDav/UreqWebDav/SyncState/SyncError/SyncOutcome/SyncProtection、SecretStore/SecretKind/SecretError、
crypto::{encrypt_config, decrypt_config, validate_password}。

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
5. 不随意修改 crates/vibekey-core、scripts/、.github/ 或覆盖 assets/icon.ico；界面/行为变化应同步更新 README 与 docs 中的对应说明。
6. 完成后在自己的最后一条消息里列：已实现文件清单、未实现/待办、编译命令与结果、风险点。
