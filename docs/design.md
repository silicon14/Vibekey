# Vibekey 设计文档

Vibekey 是一个常驻托盘的小工具：按住 Alt 后按数字键，就能把预先配置好的文字直接输入到当前窗口。
它同时提供一个可选择的片段列表、一个配置菜单，以及基于 WebDAV 的配置同步。

## 目标

- 键盘驱动、零打扰：Alt 组合键直接出字，不弹主窗口。
- 配置是纯文本 TOML，可手改、可版本管理、可 WebDAV 同步。
- Windows 7 SP1 可用（32/64 位），兼容更新一点的 Windows。
- 核心逻辑（配置、快捷键、模板展开、同步）与 Win32 前端分离，可在 Linux 上单元测试。

## 非目标

- 不做图形化片段编辑器（v1 用文本编辑器 + 设置对话框）。
- 不做多平台 GUI（Windows 之外只保留 core）。
- 不做云盘专有 API（只做标准 WebDAV，Basic 认证）。

## 目录结构

    Cargo.toml                  workspace
    crates/vibekey-core/        平台无关：配置 / 快捷键 / 展开 / 同步 / 状态
    crates/vibekey-win/         Win32 前端：钩子 / 输入 / 托盘 / 菜单 / 弹窗 / 设置
    scripts/                    交叉编译工具链与构建脚本
    docs/                       设计、Win7 说明、WebDAV 说明
    assets/                     示例配置

## 关键技术决策

### 1. 快捷键：低级键盘钩子而不是 RegisterHotKey

RegisterHotKey 注册的组合键会被系统独占，且无法感知 Alt 单独按下的时序，也无法做“按住 Alt”模式。
因此使用 WH_KEYBOARD_LL 全局钩子：

- 钩子线程只做判定，命中时返回 1 吞掉按键，然后 PostMessage 给主线程执行动作。
- 被吞掉的按键不会再传给前台程序，避免 Alt+1 触发目标程序的菜单。
- 只吞掉命中我们绑定表的按键；其余按键原样透传。
- 我们自己注入的按键带有 LLKHF_INJECTED 标志，直接忽略，避免死循环。
- 当前台窗口属于 Vibekey 自己（弹窗/设置）时，钩子透传，让 UI 正常收到按键。

代价：全局钩子会被安全软件关注，且对以管理员权限运行的前台程序无能为力（UIPI），文档里说明。

### 2. 触发方式

- immediate（默认）：Alt+1 立即触发。
- hold：Alt 按住超过 hold_ms 毫秒后才响应数字键，减少误触发（Alt+数字在部分程序里有含义）。
两种方式都只匹配绑定表里的键。

### 3. 文字注入：默认剪贴板粘贴

中文/Unicode 走剪贴板最稳：保存剪贴板文本 → 写入片段 → 发送 Ctrl+V → 延时恢复剪贴板。
可选 type 模式：SendInput(KEYEVENTF_UNICODE) 逐字符输入，适合不允许动剪贴板的场景。
注入前必须等物理 Alt 松开（否则 Ctrl+V 会变成 Ctrl+Alt+V）。

### 4. 配置

路径：%APPDATA%\Vibekey\config.toml（Linux 测试时用 XDG_CONFIG_HOME）。
没有配置文件时写入带示例片段的默认配置。
字段见 core::config；保存采用写临时文件后 rename 的原子替换。

### 5. 密码与同步

WebDAV 密码不写入 config.toml：Windows 上用 DPAPI 加密后存放在 %APPDATA%\Vibekey\secret.bin，
这样同步出去的配置文件不含秘密，换机器时需要重新输入一次密码。

同步逻辑在 vibekey-core::sync::engine，HTTP 客户端可替换（测试用内存实现）：

- 本地 hash、远端 hash 都记录在 sync-state.json，用内容 hash 判断“谁变了”，不依赖服务器是否返回 ETag。
- 只有本地变 → 用 If-Match 上传；远端变 → 下载并备份旧本地文件。
- 两边都变 → 远端先备份成 <url>.remote-backup-时间戳.toml，然后本地覆盖远端（本地优先）。
- 首次同步（无状态）且远端已有文件 → 以远端为准（新机器接管已有配置）。
- 远端文件解析失败时拒绝覆盖本地。

HTTP：ureq + rustls + 内置 webpki 根证书（不依赖系统证书库，规避 Win7 根证书过期问题），Basic 认证。

### 6. Win7 构建

Rust 1.78 起，普通 *-pc-windows-* 目标的 std 要求 Windows 10；官方为此提供了 tier 3 的
*-win7-windows-gnu / *-win7-windows-msvc 目标。本项目在 Linux 上这样构建 Win7 版本：

- llvm-mingw（MSVCRT 版，不依赖 UCRT 更新）+ Rust nightly + rust-src
- cargo +nightly build -Z build-std=std,panic_abort --target x86_64-win7-windows-gnu
- 链接参数强制 /SUBSYSTEM 6.1，并用 llvm-readobj 校验导入表只包含 Win7 已有的 DLL/函数。
- Windows 10+ 用户也可以直接用 stable 的 *-pc-windows-msvc 构建（代码只用 Win7 时代 API）。

## 配置示例

    activation = "immediate"
    hold_ms = 350
    input_method = "paste"
    restore_clipboard = true
    start_with_windows = false
    ignore_apps = ["photoshop.exe"]

    [hotkeys]
    list = "alt+`"
    config = "alt+backspace"

    [sync]
    enabled = false
    url = "https://dav.example.com/vibekey/config.toml"
    username = "me"
    interval_minutes = 15
    sync_on_change = true
    backup_on_conflict = true
    timeout_secs = 20

    [[snippet]]
    key = "1"
    name = "自我介绍"
    text = "你好，我是 {user}。"
    send_enter = false
    enabled = true

    [[snippet]]
    key = "2"
    name = "时间"
    text = "现在是 {datetime}"
    enabled = true

    [[snippet]]
    key = ""
    name = "只看列表"
    text = "这个片段没有直接快捷键，只在列表里出现"
    enabled = true

## 片段模板变量

- {date} {time} {datetime}
- {clipboard}（注入前读取的剪贴板文本）
- {user}（Windows 用户名）{host}（计算机名）
- 双写花括号输出字面花括号；未知变量原样保留。

## 已知限制

- 管理员权限窗口无法注入（UIPI），检测到后给出托盘提示。
- 全局吞掉 Alt+数字，会和个别程序（部分游戏、IDE 快捷键）冲突；用 ignore_apps 或 hold 模式规避。
- 剪贴板恢复只恢复文本；原剪贴板里的图片/文件会丢失（restore_clipboard=false 可关闭恢复）。
- WebDAV 只支持 Basic 认证；Digest/OAuth 需要后续补齐。
- 配置冲突采用本地优先策略，远端改动会备份但不会自动合并。
