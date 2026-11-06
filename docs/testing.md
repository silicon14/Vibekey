# Windows 手工测试清单

Vibekey 的 Windows 前端无法在 Linux 上运行，合并前请在真实的 Windows 7 SP1 / Windows 10/11 上按本清单验证。
每次发布建议至少跑 P0。

## 准备

1. 下载 dist/vibekey-<version>-win7-x64/vibekey.exe（32 位系统用 win32 目录）。
2. 放到不含空格和中文的目录，例如 C:\Tools\vibekey\vibekey.exe，双击运行。
3. 确认 %APPDATA%\Vibekey\ 下生成 config.toml、vibekey.log；托盘出现图标。

## P0 核心功能

- [ ] 首次启动：托盘图标存在，config.toml 含示例片段，日志无 ERROR。
- [ ] 记事本中按 Alt+1：插入示例片段文字，剪贴板内容在约 0.3 秒后恢复原状。
- [ ] 浏览器输入框 / 微信 / Word 中按 Alt+1：中文正常插入，无乱码、无丢字。
- [ ] Alt+反引号（键盘左上角 · 键）：光标附近弹出片段列表；↑↓ 移动；Enter 插入；Esc 关闭；点击其它窗口自动关闭。
- [ ] Alt+Backspace：配置菜单弹出，各菜单项可点击；菜单不会因失焦立刻消失。
- [ ] 编辑 config.toml 增加 1 个片段，保存后 2 秒内自动生效（无需重启）。
- [ ] 把 config.toml 改坏（例如少一个引号）：程序不崩溃，托盘气泡提示，旧配置继续可用；修复后自动恢复。
- [ ] 退出菜单：进程结束、托盘图标消失、日志有退出记录。
- [ ] 再次启动：单实例生效（第二个实例立即退出，不出现第二个托盘图标）。

## P0 同步（WebDAV）

- [ ] 设置窗口填写 URL/用户名/密码，点“测试连接”成功。
- [ ] 保存后触发同步，远端出现 config.toml；本机 sync-state.json 生成。
- [ ] 第二台机器（不同 APPDATA 或虚拟机）配置同一 URL：首次同步拉取远端配置。
- [ ] 机器 A 改片段 → 同步 → 机器 B 同步：B 的配置更新，旧文件保留为 config.toml.bak。
- [ ] A、B 同时改不同片段后分别同步：后同步的一方本地优先，远端生成 config.toml.remote-backup-<时间戳>.toml，
      托盘提示冲突。
- [ ] 错误密码 / 断网：同步失败但程序不崩溃，日志与托盘提示合理，恢复网络后下次同步成功。
- [ ] 远端文件被删除：下次同步重新上传本地配置。
- [ ] 密码只存在于 DPAPI 加密的 secret.bin，config.toml 中没有任何明文密码；换机器需要重新输入。

## P1 体验与兼容

- [ ] Alt 按住模式（设置里切换为“按住 Alt”）：快速 Alt+1 不触发；按住 Alt 超过 hold_ms 再按 1 触发。
- [ ] ignore_apps 中加入 notepad.exe：记事本中 Alt+1 不再触发（但列表热键仍可用？按设计只忽略片段/菜单触发）。
- [ ] 在管理员权限运行的记事本/任务管理器中按 Alt+1：不会注入，给出“权限更高”的提示。
- [ ] 目标程序自己使用 Alt+数字（例如某些 IDE、游戏）：用 hold 模式或 ignore_apps 规避；确认不会误吞其它按键。
- [ ] 输入法处于中文状态时 Alt+1：插入正常，输入法候选框不会卡住。
- [ ] 多显示器 / 125% 或 150% DPI：列表弹窗出现在光标附近且不超出屏幕，文字不模糊。
- [ ] 屏幕锁定/切换用户后：钩子与托盘仍工作。
- [ ] Explorer 重启（任务管理器结束 explorer.exe 再启动）：托盘图标自动恢复。
- [ ] 开机启动勾选后重启：自动运行且只有一个实例；取消勾选后重启不再自启。
- [ ] 退出时若有同步在跑：进程能正常结束（最多等几秒）。

## P0 Win7 专项

- [ ] Windows 7 SP1 x64 上运行 x64 版；32 位 Win7 SP1（或 WoW64 下运行 x86 版）均可启动。
- [ ] 未安装 UCRT / .NET 的干净 Win7 可运行（exe 只依赖系统 DLL）。
- [ ] 用 llvm-readobj 校验产物：Subsystem 6.1，无 api-ms-win-* / ucrtbase / libunwind.dll 导入
      （scripts/verify-win7-pe.sh 自动检查）。
- [ ] HTTPS WebDAV（Nextcloud 等）能在 Win7 上完成 TLS 握手（rustls 内置根证书，不依赖系统根证书）。
- [ ] 中文用户名路径（%APPDATA% 含中文）下配置与日志正常读写。

## 本地 WebDAV 测试环境

快速起一个标准 WebDAV 服务器（任选其一）：

    # 方式 1: rclone（跨平台）
    rclone serve webdav --addr 127.0.0.1:8080 --user test --pass test /tmp/vibekey-dav

    # 方式 2: Python wsgidav
    pip install wsgidav cheroot
    wsgidav --host=127.0.0.1 --port=8080 --root=/tmp/vibekey-dav --auth=accept

然后设置：
    sync.url = "http://127.0.0.1:8080/config.toml"
    sync.username = "test"

注意：wsgidav 的 accept 模式不校验密码，仅用于功能验证。

## 已知限制（发布说明中必须保留）

- 无法向以管理员身份运行的程序注入文字（Windows UIPI 限制）。
- 全局 Alt+数字可能与个别程序快捷键冲突，使用 hold 模式或 ignore_apps。
- 剪贴板恢复仅恢复文本格式；原剪贴板为图片/文件时不会恢复（可关闭 restore_clipboard）。
- Win7 已于 2020 年停止官方支持，请仅在隔离/内网环境使用；网络同步建议仅使用 HTTPS + 应用密码。
