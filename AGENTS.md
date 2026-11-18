# AGENTS.md — Vibekey 交接与开发说明

> 这份文件写给**下一个接手本项目的 AI agent 或开发者**，默认你将在 **Windows** 上继续工作。
> 先读本文件，再读 README.md、docs/spec-win-app.md、docs/testing.md。

---

## 0. 先看这里（TL;DR）

- **项目是什么**：Vibekey 是一个常驻托盘的 Windows 小工具。按住 Alt 后按数字键，把预设文字直接输入当前窗口；
  Alt+反引号打开单行片段列表，Alt+V 打开内存文本剪贴板历史，Alt+Backspace 打开配置菜单；配置是纯文本 TOML；支持 WebDAV 同步。
- **当前状态**：功能已完整实现，源码在仓库里；core 有 49 个单元测试 + 18 个 WebDAV/TLS 集成测试全绿；
  Win7 x64/i686 的 exe 已用 Linux 交叉编译产出，PE 头/导入表已做静态校验。
- **真机验证进度**（2026-11-06，Windows 11 26200 x64）：已确认启动 / 托盘图标 / 右键菜单 /
  双击设置窗口 / 单实例 / 退出 / 真键盘 Alt+1 插入全部正常，记录见 `docs/testing-win11-2026-11-06.md`。
  仍待验证：Alt+反引号列表、hold 模式、剪贴板恢复、管理员权限窗口、多显示器 DPI、Win7、中文输入法下插入。
- **Windows 本机构建已跑通**（2026-11-06）：rustup stable 1.99.1 MSVC + VS Build Tools 18 + SDK 10.0.26100；
  `cargo test -p vibekey-core` 49 unit + 18 integration 全绿，`cargo build -p vibekey` 约 10 秒出 debug exe。
- **在 Windows 上第一件事**：
  1. 用 stable MSVC 构建 debug 版（仅 Win10/11 跑）；
  2. 按 docs/testing.md 的 P0 清单逐项验证；
  3. 发现问题后按本文件第 8 节的流程修改、测试、提交。
- **产物**：dist/ 下是 Linux 交叉编译出的 Win7 发布版（便携单文件），可直接在 Win7 SP1 上试。
- **技术栈**：Rust + windows-sys 0.59（裸 Win32 API）+ ureq/rustls。没有 electron/tauri/egui/WebView2/OpenGL 依赖。
  `vibekey-win/build.rs` 用 embed-resource 2.5.2 嵌入 exe 图标和版本信息（只构建期依赖，不增加运行库）。
- **Win7 兼容的关键**：普通 Rust 目标的预编译 std 要求 Windows 10，本项目用 tier-3 的
  x86_64-win7-windows-msvc / i686-win7-windows-msvc 目标 + nightly 的 -Z build-std 构建 Win7 版本。
  细节见 docs/win7.md。

---

## 1. 为什么项目是在 Linux 上写的（交接背景）

开发环境是 Linux，而目标平台是 Windows。为了让代码在没有 Windows 机器的情况下也能推进，采用了：

- 平台无关逻辑（配置、快捷键解析、模板展开、WebDAV 同步）抽到 crates/vibekey-core，可在 Linux 上单测；
- Windows 前端用 windows-sys 0.59 直接调 Win32 API，不依赖任何跨平台 GUI 框架；
- Win7 产物用 llvm-mingw（MSVCRT 版）+ Rust nightly + build-std 在 Linux 交叉编译；
- 两轮独立代码审查发现的问题已修复（hold 模式、剪贴板数据丢失、钩子退出死锁、菜单模式、前台激活等）。

**这仍然是一个折衷**：Windows 上的运行时行为（焦点、激活、钩子时序、托盘、输入法、剪贴板）没有被真机验证过。
所以你接手后的工作重点是“真机验证 → 修 bug → 回归”，而不是重写架构。

---

## 2. 项目速览

### 功能

- Alt+1 … Alt+9、Alt+0、Alt+-、Alt+=：12 个槽位直接插入片段文字
- Alt+反引号（1 左边那颗 · 键）：可搜索的单行片段列表（方向键/Enter/Esc/单击，F2/右键快速编辑，☰ 完整菜单）
- Alt+V：类似 Win+V 的文本剪贴板历史（最近 50 条、搜索、Enter/单击粘贴、Delete、清空；仅内存，退出清空）
- Alt+Backspace 或右键托盘：配置菜单（列表、历史、新建片段、设置、TOML、重载、同步、启动、日志、关于、退出）
- 统一设置中心：图形化片段库（搜索/新建/复制/删除/上移下移排序/快速编辑）、全部输入行为与全局热键、WebDAV 同步与备份
- 模板变量：{date}、{time}、{datetime}、{clipboard}、{user}、{host}
- WebDAV 同步：明文内容哈希判冲突，本地优先 + 远端备份；可选 PBKDF2 + AES-256-GCM 内容加密与防降级；
  WebDAV 登录密码和独立配置加密密码分别用 DPAPI 保存，不随配置同步
- 单实例、托盘气泡、日志轮转、配置热重载、配置原子写

### 目录

    crates/vibekey-core/       平台无关：config / keymap / expand / crypto / sync / secret / state
    crates/vibekey-win/        Win32 前端：app / history / hook / inject / sync / secret / autostart / log / util + ui/{tray,menu,popup,settings,theme}
    docs/                      设计、Win7 构建、配置、WebDAV、测试清单
    scripts/                   Linux 交叉编译/打包/校验脚本（bash）
    assets/config.example.toml 完整示例配置
    dist/                      生成物（git 忽略）：Win7 发布目录 + SHA256SUMS
    .github/workflows/build.yml CI（尚未在真实 GitHub 上跑过）

### 关键文档

- README.md — 用户视角的功能、安装、配置、FAQ
- docs/design.md — 架构与技术取舍
- docs/config.md — TOML 逐字段参考
- docs/webdav.md — WebDAV 地址、同步语义、冲突策略
- docs/win7.md — Win7 构建原理、PE 校验、系统要求
- docs/testing.md — **Windows 真机验收清单（交接后必跑）**
- docs/spec-win-app.md — Win32 前端的实现规范

---

## 3. 在 Windows 上构建

### 3.1 日常开发 / 调试（Windows 10/11，stable MSVC）

适合快速迭代和真机调试。产物**不能**在 Win7 上运行。

1. 安装 rustup（stable，默认 x86_64-pc-windows-msvc）。
2. 安装 Visual Studio Build Tools，勾选 “Desktop development with C++”（MSVC + Windows SDK）。
3. 命令：

    cargo test -p vibekey-core
    cargo build -p vibekey
    target\debug\vibekey.exe

也可以直接 cargo run -p vibekey（GUI 程序，无控制台输出；日志见 %APPDATA%\Vibekey\vibekey.log）。

### 3.2 Win7 发布构建（推荐：nightly + build-std，MSVC）

Win7 目标（x86_64-win7-windows-msvc / i686-win7-windows-msvc）是 tier-3，官方不提供预编译 std，
必须用 nightly 的 build-std 从源码编译 std。前提是 3.1 的 MSVC 工具链已装好。

    rustup toolchain install nightly --profile minimal --component rust-src

    # 必须静态链接 CRT 并固定子系统 6.1；否则干净 Win7 会缺 VCRUNTIME/UCRT
    set RUSTFLAGS=-C target-feature=+crt-static -C link-arg=/SUBSYSTEM:WINDOWS,6.01

    # 64 位
    cargo +nightly build --release -Z build-std=std,panic_abort ^
        --target x86_64-win7-windows-msvc -p vibekey

    # 32 位
    cargo +nightly build --release -Z build-std=std,panic_abort ^
        --target i686-win7-windows-msvc -p vibekey

产物在 target\<triple>\release\vibekey.exe。

**校验 Win7 兼容性**（发布前必做）：

1. 子系统版本必须 ≤ 6.1（win7-msvc 目标默认 6.01）：

       dumpbin /headers target\x86_64-win7-windows-msvc\release\vibekey.exe | findstr /i "subsystem version"

2. 导入表里不能出现 Win8+ 才有的东西（api-ms-win-*、ucrtbase.dll、VCRUNTIME*、libunwind.dll 等）。
   如果机器上有 llvm-readobj（MSYS2 的 mingw-w64 工具链自带），可以：

       llvm-readobj --file-headers target\x86_64-win7-windows-msvc\release\vibekey.exe
       llvm-readobj --coff-imports target\x86_64-win7-windows-msvc\release\vibekey.exe

   在 Git Bash / WSL 里也可以直接用仓库里的 scripts/verify-win7-pe.sh（见 3.4）。

> 注意：不要用 cargo build --release（stable MSVC）冒充 Win7 产物——从 Rust 1.78 起它的 std 要求 Windows 10。

### 3.3 Win7 发布构建（备选：MSYS2 MinGW64 + windows-gnu）

如果你更习惯 GCC 工具链：

1. 安装 MSYS2，打开 “MSYS2 MinGW 64-bit” shell，安装：

       pacman -S mingw-w64-x86_64-gcc

2. 安装 Rust nightly + rust-src（可用 rustup 的 Windows 安装器，在同一个 shell 里能访问 cargo）。
3. 构建：

       set CARGO_TARGET_X86_64_WIN7_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc
       set RUSTFLAGS=-C link-arg=-Wl,--major-subsystem-version=6 -C link-arg=-Wl,--minor-subsystem-version=1
       cargo +nightly build --release -Z build-std=std,panic_abort ^
           --target x86_64-win7-windows-gnu -p vibekey

   32 位在 “MSYS2 MinGW 32-bit” shell 里用 i686-w64-mingw32-gcc / i686-win7-windows-gnu。
   （仓库 .cargo/config.toml 默认把 GNU 目标的 linker 指到 llvm-mingw 的 clang；上面的环境变量会覆盖它。）

### 3.4 Linux / WSL 交叉编译（当前 dist/ 的生成方式）

如果你在 Windows 上有 WSL 或 Git Bash + Linux 工具链，也可以沿用仓库脚本。脚本是 bash，需要 bash 环境：

    scripts/install-toolchain.sh     # llvm-mingw MSVCRT + nightly/rust-src，装到 ~/toolchains（无需 root）
    . scripts/dev-env.sh
    scripts/build-win.sh x86_64-win7-windows-gnu
    scripts/build-win.sh i686-win7-windows-gnu
    scripts/verify-win7-pe.sh target/x86_64-win7-windows-gnu/release/vibekey.exe
    scripts/package-release.sh        # 生成 dist/ + SHA256SUMS

工具链版本/目录可用 VIBEKEY_LLVM_MINGW_VERSION、VIBEKEY_TOOLCHAIN_DIR 覆盖。

---

## 4. 测试与验收

### 4.1 自动测试

    cargo test -p vibekey-core        # 49 unit + 18 integration（WebDAV/TLS 用 127.0.0.1 进程内 mock）
    cargo fmt --all -- --check
    cargo build -p vibekey            # Win10+ 本机

Win7 发布前再跑 3.2 的两个 target 构建 + PE 校验。CI 见 .github/workflows/build.yml
（包含 core-test / win7-cross / windows-native 三个 job，但尚未在真实 GitHub 上执行过）。

### 4.2 真机验收（必做）

按 docs/testing.md 的 P0 清单执行。特别优先验证这些与近期修复相关的项：

1. 记事本 / 资源管理器 / Office 里 Alt+1：文字插入文档，**目标程序的菜单不会被激活**；
2. Alt+反引号弹出列表后，**不点鼠标**直接方向键/Enter/Esc 可用；
3. 复制一个文件/图片后触发片段：文字以逐字符输入插入，**剪贴板里的文件/图片不丢**；
4. 连续快速触发两个不同片段：插入文字正确，剪贴板最终恢复为用户原文；
5. 设置里切 hold 模式：快速点按 Alt+1 不触发；按住 Alt 约 1 秒再按 1 才触发；
6. 按住 Alt+1 不放：只插入一次，目标程序不 beep、不重复出字；
7. 托盘：左键单击（列表）、双击（设置）、右键（菜单）；退出后托盘图标消失；
8. 管理员权限窗口：给出“无法向管理员权限窗口输入”的提示（而不是静默无反应）；
9. 配置写坏、改配置热重载、WebDAV 同步（可用本机 rclone/wsgidav 起服务，见 testing.md）；
10. Windows 7 SP1 x64 和 x86 各跑一遍；Win10/11 也跑一遍。

### 4.3 本地 WebDAV 测试

    # rclone（Windows 版）
    rclone serve webdav --addr 127.0.0.1:8080 --user test --pass test C:\Temp\vibekey-dav

    # 或 Python wsgidav
    pip install wsgidav cheroot
    wsgidav --host=127.0.0.1 --port=8080 --root=C:\Temp\vibekey-dav --auth=accept

配置里：

    [sync]
    enabled = true
    url = "http://127.0.0.1:8080/config.toml"
    username = "test"

---

## 5. 代码地图与关键设计（改代码前必读）

### 5.1 核心模块

| 模块 | 职责 |
| --- | --- |
| core::config | TOML 配置模型、默认值、校验、原子读写、配置目录 |
| core::keymap | 快捷键解析（alt+1 / alt+backspace / alt+反引号）、Alt 槽位绑定、冲突检测 |
| core::expand | 片段模板变量展开 |
| core::crypto | 版本化配置加密信封、PBKDF2-HMAC-SHA256 600k、AES-256-GCM、认证/篡改失败关闭 |
| core::sync | 明文哈希同步引擎、格式迁移/密码轮换/防降级 + ureq 客户端 + 状态持久化 |
| core::secret | 平台密码存储抽象（Windows 由 win 端用 DPAPI 实现） |
| win::app | 单实例、隐藏主窗口、消息循环、托盘/菜单/弹窗/设置调度、热重载、退出 |
| win::hook | 独立线程的 WH_KEYBOARD_LL 全局钩子、Alt 状态、hold 计时、吞键 |
| win::inject | 注入工作线程：等 Alt 松开、菜单模式处理、剪贴板粘贴或 Unicode 输入；内部写入经 HistoryControl 屏蔽 |
| win::history | 最近 50 条内存文本历史、稳定 ID、去重；剪贴板互斥与 sequence 前后复核，不写磁盘 |
| win::ui::tray | 托盘图标（嵌入 ICO）、气泡、TaskbarCreated 重建 |
| win::ui::menu | TrackPopupMenu 配置菜单 |
| win::ui::popup | 片段/历史共用的单行自绘选择器、F2/右键编辑、Delete/清空历史（ListBox/Edit 子类化） |
| win::ui::settings | 三页设置中心与完整图形化片段编辑器；工作副本校验后原子保存 |
| win::ui::theme | 共享中文字体、配色、画刷和 GDI 绘图工具 |
| win::sync | 同步后台线程（mpsc + recv_timeout） |
| win::secret / autostart / log / util | DPAPI、注册表 Run、日志、Win32 小工具 |

### 5.2 硬性约束（不要破坏）

- **只用 windows-sys 0.59 + std**，GUI 用裸 Win32。**不要**引入 nwg/egui/tauri/winit/WebView2/OpenGL
  依赖（Win7 兼容 + 体积 + 无额外运行库的要求）。
- **只使用 Windows 7 已存在的 API**。禁止静态使用 Win8+ API（如 SetProcessDpiAwarenessContext、
  GetDpiForWindow、SetThreadDescription、GetSystemTimePreciseAsFileTime、WaitOnAddress 等）。
  如果确实需要新 API，必须 GetProcAddress 动态加载并做降级。
- 发布配置 panic=abort、GUI 子系统；**不允许跨 FFI 的 panic**。所有 unsafe 块写 // SAFETY:。
- 用户可见文案中文；日志可以英文。
- **主线程规则**：Window Proc 只在主线程运行；App 状态只允许主线程访问；不要跨会泵消息的调用
  （TrackPopupMenu / MessageBox / DestroyWindow）持有 &mut App。当前 app.rs 顶部注释记录了残余的
  严格别名 caveat；如需彻底消除，要把 App 改成内部可变性 + 重入消息队列（属于 P1 重构）。
- 32/64 位都要能编译：SetWindowLongPtrW/SetWindowLongW 分支；NOTIFYICONDATAW 在 x86 是 packed(1)，
  写字符串字段必须 addr_of_mut! + write_unaligned。
- 不要改配置格式而不更新 assets/config.example.toml、docs/config.md、README。
- 不要记录明文密码；不得全局关闭 TLS 校验。自签名仅允许 sync.certificate_sha256 指定 DER 指纹固定，并继续验证握手签名；DPAPI 登录密文只在 secret.bin，配置加密 current/previous 只在 sync-encryption.bin。
- 加密同步的 hash 必须基于解密后明文；每个 PUT（含冲突备份、412、迁移）都必须统一 encode。已加密远端不得在启用加密时接受明文降级；默认 false 也不得在新机器/明文基线上隐式覆盖密文。
- 密码 previous 只有在新密文可用且 state.save 成功后才能按本轮密码对条件清理；DPAPI 读校验写必须持同一 Mutex；pending 时禁止第三次换密码，不得让旧后台任务覆盖设置页刚写入的新轮换。

### 5.3 几个容易踩的坑（都已处理，改代码时别破坏）

- **钩子吞键**：Alt+数字被吞后，目标窗口只看到 Alt down/up，会进入菜单模式；inject 端会检测
  GUI_INMENUMODE 并先发 Esc。改注入流程时不要删这段。
- **hold 模式计时**：Windows 会重复发送 Alt 的 keydown，只有 ALT_PRESSED 0→1 边沿才记录时间。
- **自动重复**：CONSUMED_VK 记录被吞的键，其重复事件必须继续吞掉。
- **前台激活**：普通 SetForegroundWindow 在用户刚按键时会被前台锁拒绝；弹窗/菜单用
  util::force_foreground()（AttachThreadInput 技巧）。
- **剪贴板**：restore_clipboard=true 时先 clipboard::inspect()：纯文本才保存/恢复；文件/图片/富文本
  自动改用 SendInput Unicode，不覆盖剪贴板。恢复在注入线程内串行执行（250ms 后）。
- **同步**：内容 hash + ETag 判定；本地优先，远端冲突备份为 <url>.remote-backup-<时间戳>.toml；
  首次同步（无状态）且远端有文件时以远端为准；远端配置解析失败拒绝覆盖本地。
- **退出**：同步线程 busy 时最多等 2s 后 detach，避免关机卡死。

---

## 6. 已知未验证风险与待办（按优先级）

### P0：真机验证（不做就无法发布）

- [x] Windows 11 x64：启动、托盘、右键菜单、设置窗口、单实例、退出、真键盘 Alt+1 插入（2026-11-06）
- [ ] Win7 SP1 x64 / x86 上启动、托盘、Alt+数字、列表、配置菜单、设置窗口、同步全部按 testing.md 跑通
- [ ] 菜单模式 / 前台激活 / 剪贴板 / hold 模式 / 自动重复这几处修复在真实程序（记事本、资源管理器、Office、微信、浏览器）中确认
- [ ] 正常桌面里的真实复制 → Alt+V → 搜索/粘贴/Delete/清空、内部写入不污染历史、退出无历史文件（2026-11-10 自动化会话 OpenClipboard/clip.exe 均被 error 5 拒绝，debug 样本仅覆盖 UI）
- [ ] 输入法为中文时插入是否正常
- [ ] 管理员权限窗口、UIPI 行为
- [ ] 多显示器 + 125%/150% DPI 下列表位置
- [ ] 32 位 exe 真机

### P1：已知问题/可改进

- [x] 菜单模式检测已改为固定目标线程、150ms 沉降 + Esc 后 250ms 有界轮询；前台变化取消注入。真实 Explorer 地址栏场景仍需人工复验
- [ ] AttachThreadInput 失败时列表只能鼠标操作：可增加托盘气泡提示或在 show 后再次重试激活
- [ ] App 严格别名 caveat（app.rs 顶部注释）：彻底修需要内部可变性 + 重入队列重构
- [x] 设置窗口已加入“清除已保存密码”（2026-11-09；点击后仅在应用时清除，取消不影响原密码）
- [ ] UIPI 查询失败（返回 None）时按不可确认处理；可优化提示
- [ ] 非文本剪贴板时改逐字输入，长文本较慢；如需更快可考虑 OLE IDataObject 完整快照（复杂，谨慎）
- [ ] 菜单 owner 是隐藏窗口，极端前台锁场景下菜单可能需点击空白才消失（实测确认）
- [ ] 代码签名 / SmartScreen 提示（需要证书，非代码问题）
- [ ] WebDAV 只支持 Basic 认证；Digest/OAuth 未实现。内容加密不保护 Basic 头/元数据，不防旧密文回滚，仍需 HTTPS
- [ ] CI 未在真实 GitHub 跑过；跑通后把 badge / artifact 用法补进 README

### P2：可选增强

- [x] 图形化片段编辑器与弹窗快速编辑（2026-11-09；仍可用 TOML，使用说明见 README 和 docs/testing.md）
- [ ] 同步支持双向合并（当前本地优先 + 备份）
- [ ] 安装包/自更新/多语言
- [ ] 更多 core 单元测试（keymap 边界、同步状态机异常路径）

---

## 7. 调试方法（Windows）

- 日志：%APPDATA%\Vibekey\vibekey.log，超过 1 MB 轮转；菜单 →「打开日志文件」。
- 更详细日志：设置环境变量 VIBEKEY_LOG=debug 后重启程序。
- 配置文件：%APPDATA%\Vibekey\config.toml；同步状态 sync-state.json；DPAPI 登录密码 secret.bin；配置加密密码 sync-encryption.bin。
- 单实例：同时只能运行一个；测试多配置时先退出，或换 Windows 用户/沙盒。
- 钩子装不上：多半被安全软件拦截，日志会记录 SetWindowsHookExW 错误码。
- 常见问题排查表见 README「排查问题」。
- 快速看 PE：

      dumpbin /headers vibekey.exe | findstr /i "subsystem"
      llvm-readobj --coff-imports vibekey.exe

---

## 8. 开发流程（Agent 约定）

1. **改代码前**：读 docs/spec-win-app.md 相关章节 + 本文件第 5 节；确认改动范围。
2. **改完**：
   - cargo fmt --all
   - cargo test -p vibekey-core
   - 至少 cargo build -p vibekey（Win10+）通过；
   - 如果改动涉及 Win32/核心行为，尽量再跑 3.2 的两个 target release 构建 + PE 校验；
3. **更新文档**：行为/配置/构建/已知限制发生变化时，同步更新 README、docs/*。
4. **提交**：一个逻辑改动一个 commit；提交信息中英均可；不要把 target/、dist/ 提交进 git。
5. **发布**：Win7 产物用 3.2 或 3.4 的方式构建，PE 校验通过、真机 P0 通过后再交。
6. **不要**：
   - 不要用 stable MSVC release 冒充 Win7 版；
   - 不要引入需要 UCRT/VC++ redist/WebView2 的依赖；
   - 不要修改 core 公共 API 而不检查 win 端调用；
   - 不要把密码/密钥写进配置、日志、测试或提交历史。

---

## 9. 交接后 30 分钟检查清单

- [ ] 装好 rustup + VS Build Tools；cargo test -p vibekey-core 全绿
- [ ] cargo build -p vibekey，运行 target\debug\vibekey.exe，确认托盘出现
- [ ] 记事本里 Alt+1、Alt+反引号、Alt+Backspace 各试一次
- [ ] 打开设置窗口，确认控件/中文/保存/重开正常
- [ ] 按 docs/testing.md P0 逐条记录结果（通过/失败/日志）
- [ ] 失败项按第 6 节优先级修，修完重复第 8 节流程
- [ ] Win7 机器上跑 dist/ 里的 exe，或按 3.2 自己构建并 dumpbin 校验
- [ ] 把验证结果补进 docs/testing.md 或新开 issue/待办

---

## 10. 当前产物与提交状态（2026-11-06 交接时）

- **源码与完整 git 历史最初在开发机的局域网 git 服务上**（分支 master）。
  Windows 工作区是用 `git clone` 拉回来的副本。原先工作区里那份 `.git` 是坏的（index 有条目、object 库为空），
  已挪到 `C:\SAPrograms\vibekey-git-broken-20261106`。
- 分支 master；公开仓库为 GitHub `silicon14/Vibekey`（remote `github`），`origin` 仍指向局域网镜像。
- 最近提交：修复两轮 QA 问题后的版本（见 git log）。
- dist/（git 忽略）：

      dist/vibekey-0.1.0-win7-x64/vibekey.exe
      dist/vibekey-0.1.0-win7-win32/vibekey.exe
      dist/SHA256SUMS

  当前 SHA256：
  - x64: 376e96371f8bce3ecfeab257d3c7e4871316c53a8f94c13dface7b59abc39bfc
  - x86: a2b3a94e7e391f179b20b47d61ffeffefcd3c624108472697a26021a4d0d10a3

  这些是 Linux 交叉编译产物，PE 静态校验通过，但未在真实 Windows 上运行过。

- 开发环境（生成这些产物的环境）：Linux，rustc 1.99.1（stable）、nightly 1.101，
  llvm-mingw 20261104（MSVCRT，ubuntu-22.04-x86_64）。

---

## 11. 参考

- 用户文档：README.md
- 设计：docs/design.md
- 配置：docs/config.md
- WebDAV：docs/webdav.md
- Win7：docs/win7.md
- 真机测试：docs/testing.md
- Win32 实现规范：docs/spec-win-app.md
- CI：.github/workflows/build.yml
