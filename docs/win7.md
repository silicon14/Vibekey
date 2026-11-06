# Windows 7 支持说明

Vibekey 的目标平台是 **Windows 7 SP1**（32/64 位）以及更新的 Windows。
Windows 7 已经于 2020 年 1 月停止官方支持，本项目的 Win7 支持属于“尽量兼容”，
请仅在隔离环境 / 内网使用，并注意文末的安全提醒。

本文说明 Win7 版本为什么需要特殊的构建方式、如何从源码构建、如何校验产物，
以及 Win7 上需要哪些系统补丁。

## 为什么不能直接用普通 Windows 目标

从 **Rust 1.78** 开始，官方预编译 std 的 `*-pc-windows-*` 目标（例如
`x86_64-pc-windows-msvc`、`x86_64-pc-windows-gnu`）最低要求 **Windows 10**：
std 里用到了 Win8/Win10 才有的系统 API，编出来的程序在 Win7 上一启动就会失败。

Rust 为此提供了 **tier 3** 的 Win7 目标：

| 目标 | 说明 |
| --- | --- |
| `x86_64-win7-windows-gnu` | 64 位，GNU ABI，Linux 交叉编译用 |
| `i686-win7-windows-gnu` | 32 位，GNU ABI，Linux 交叉编译用 |
| `x86_64-win7-windows-msvc` | 64 位，MSVC ABI（需要 Windows + VS 工具链） |
| `i686-win7-windows-msvc` | 32 位，MSVC ABI |

tier 3 的含义是：Rust 官方**不提供**这些目标的预编译 std，也不保证 CI 覆盖，
所以必须：

1. 使用 **nightly** 工具链（`-Z build-std` 是 nightly 特性）；
2. 安装 **rust-src** 组件（提供 std 源码）；
3. 用 `-Z build-std=std,panic_abort` 从源码编译 std 和 panic_abort，其余依赖照常从 crates.io 取。

因此本项目发布 Win7 产物统一用 Linux 交叉编译：

~~~
cargo +nightly build --release -Z build-std=std,panic_abort \
    --target x86_64-win7-windows-gnu -p vibekey
~~~

（更推荐直接调用 `scripts/build-win.sh`，见下文。）

## 为什么用 llvm-mingw 的 MSVCRT 版本

Rust 的 GNU 目标需要一个 C 运行库和链接器。项目的选择是
[llvm-mingw](https://github.com/mstorsjo/llvm-mingw) 的 **MSVCRT** 变体：

- **MSVCRT 而不是 UCRT**：UCRT（Universal C Runtime）在干净的 Windows 7 上并不存在，
  需要安装 KB2999226 更新；而 `msvcrt.dll` 从 Windows 7 起就是系统自带。
  用 MSVCRT 变体编出来的 exe 不依赖任何 VC++ 运行库或 UCRT 更新。
- **llvm-mingw 自带 clang + lld + compiler-rt + libunwind**，一个压缩包即可，
  不需要再装 MinGW-w64、binutils。
- Rust 的 `*-windows-gnu` 目标在链接时仍会请求 GNU 运行库 `-lgcc` / `-lgcc_eh`，
  而 llvm-mingw 提供的是 compiler-rt 的内建函数库和 libunwind。
  `scripts/install-toolchain.sh` 会在两个 sysroot 里建立兼容符号链接：

  ~~~
  libgcc.a    -> lib/clang/*/lib/windows/libclang_rt.builtins-<arch>.a
  libgcc_eh.a -> <arch>-w64-mingw32/lib/libunwind.a
  ~~~

  没有这两个符号链接，链接阶段会报 `unable to find library -lgcc`。
- 链接参数强制子系统版本 6.1：`scripts/dev-env.sh` 会设置

  ~~~
  -C link-arg=-Wl,--major-subsystem-version=6
  -C link-arg=-Wl,--minor-subsystem-version=1
  ~~~

  让 PE 头里的 `MajorSubsystemVersion`/`MinorSubsystemVersion` 为 6.1。
  不设置时，新版 lld 的默认值可能高于 6.1，Win7 会拒绝加载。

## 从 Linux 构建 Win7 版本

一次性安装工具链（下载 llvm-mingw、nightly Rust + rust-src、建兼容符号链接）：

~~~
scripts/install-toolchain.sh
~~~

然后构建。`scripts/build-win.sh` 默认构建 64 位，传入 target 名构建 32 位：

~~~
. scripts/dev-env.sh
scripts/build-win.sh x86_64-win7-windows-gnu   # target/x86_64-win7-windows-gnu/release/vibekey.exe
scripts/build-win.sh i686-win7-windows-gnu     # 32 位
~~~

等价的原始 cargo 命令：

~~~
. scripts/dev-env.sh
cargo +nightly build --release -Z build-std=std,panic_abort \
    --target x86_64-win7-windows-gnu -p vibekey
cargo +nightly build --release -Z build-std=std,panic_abort \
    --target i686-win7-windows-gnu -p vibekey
~~~

做完整的发布目录（两个架构、示例配置、README、文档、SHA256SUMS、自动校验）：

~~~
scripts/package-release.sh
# 输出：
#   dist/vibekey-<version>-win7-x64/
#   dist/vibekey-<version>-win7-win32/
#   dist/SHA256SUMS
~~~

工具链、llvm-mingw 版本不匹配时可以用环境变量覆盖：
`VIBEKEY_LLVM_MINGW_VERSION`、`VIBEKEY_TOOLCHAIN_DIR`
（默认分别是 `20261104` 和 `~/toolchains`）。

## 从 Windows 本机构建

Windows 10/11 上安装 stable Rust + Visual Studio Build Tools 即可直接构建（**仅限 Win10+**）：

~~~
cargo build --release -p vibekey
# target/release/vibekey.exe，x86_64-pc-windows-msvc
~~~

注意这个 MSVC/stable 产物**不能**在 Win7 上运行（原因见第一节）。要在 Windows 上
编出 Win7 版本，需要 nightly + rust-src + `-Z build-std`，并使用
`x86_64-win7-windows-msvc` 目标；本项目的发布脚本和 CI 走的是 Linux + llvm-mingw
的 gnu 路线，msvc 路线未纳入自动验证。

## 校验产物是否兼容 Win7

### 自动校验（推荐）

~~~
scripts/verify-win7-pe.sh dist/vibekey-0.1.0-win7-x64/vibekey.exe
# 或者一次校验整个发布目录
scripts/verify-win7-pe.sh dist/
~~~

脚本用 llvm-mingw 自带的 `llvm-readobj` 检查，输出每个文件的 PASS/FAIL，
全部通过时退出码为 0，否则为 1。检查项：

1. PE 头 `MajorSubsystemVersion.MinorSubsystemVersion <= 6.1`；
   GUI 子系统（release 产物）；出现控制台子系统只警告。
2. 导入表里每个 DLL 都在 Win7 自带 DLL 白名单内，且不出现：
   - `api-ms-win-*` / `ext-ms-win-*`（UCRT 的 API set 桩 DLL）
   - `ucrtbase.dll`（UCRT，干净 Win7 没有）
   - `vcruntime*.dll` / `msvcp*.dll` / `concrt*.dll`（VC++ 运行库）
   - `libunwind.dll` / `libgcc_s_*.dll` / `libwinpthread-1.dll`
3. 不导入只存在于 Windows 8+ 的函数（例如 `GetSystemTimePreciseAsFileTime`、
   `SetThreadDescription`、`GetDpiForWindow`、`WaitOnAddress` 等）。

### 手动校验

工具链安装后，`llvm-readobj` 在
`~/toolchains/llvm-mingw-<版本>-msvcrt-*/bin/` 下（或 `. scripts/dev-env.sh` 之后直接在 PATH 里）：

~~~
llvm-readobj --file-headers target/x86_64-win7-windows-gnu/release/vibekey.exe \
    | grep -iE 'Subsystem|MajorSubsystemVersion|MinorSubsystemVersion'
# 期望：
#   MajorSubsystemVersion: 6
#   MinorSubsystemVersion: 1
#   Subsystem: IMAGE_SUBSYSTEM_WINDOWS_GUI (0x2)

llvm-readobj --coff-imports target/x86_64-win7-windows-gnu/release/vibekey.exe \
    | grep 'Name:'
# 期望只出现 Win7 自带的 DLL，例如：
#   Name: KERNEL32.dll
#   Name: USER32.dll
#   Name: SHELL32.dll
#   Name: ADVAPI32.dll
#   Name: msvcrt.dll
#   Name: WS2_32.dll
#   Name: bcrypt.dll
~~~

失败时的常见含义：

| 现象 | 含义 |
| --- | --- |
| SubsystemVersion 大于 6.1 | 链接参数没生效（没有 source `scripts/dev-env.sh`，或 RUSTFLAGS 被覆盖） |
| 出现 `api-ms-win-crt-*.dll` | 错误地链接了 UCRT，目标/工具链选错 |
| 出现 `ucrtbase.dll` | 同上；干净 Win7 会报缺少 DLL |
| 出现 `libunwind.dll` | 动态链接了 libunwind，应静态链接 |
| 出现 `VCRUNTIME140.dll` | 混入了 MSVC 运行库依赖 |
| 出现 `libwinpthread-1.dll` | 混入了 pthread 动态库 |

## Windows 7 系统要求与补丁

- **必须 Windows 7 SP1**（Service Pack 1）。Rust 的 win7 目标和本项目只针对 SP1；
  未打 SP1 的 RTM 版不受支持。
- **不需要**安装 UCRT 更新（KB2999226），**不需要** VC++ Redistributable，
  **不需要** .NET Framework —— 发布产物是单个 exe，只依赖系统自带 DLL。
- 32 位系统用 `win7-win32` 目录的 exe；64 位系统两个都能用（32 位版通过 WoW64 运行）。
- 与签名相关的 KB4474419（SHA-2 支持）只影响系统验证数字签名；本项目 exe 未签名，
  不装也能运行，但建议把系统更新打全。
- **HTTPS/TLS**：Vibekey 使用 rustls + 内置的 webpki 根证书，不依赖 Windows 的
  SChannel / 系统根证书库，因此 Win7 上“启用 TLS 1.2”的注册表补丁（KB3140245）
  **不是必需的**，访问 Nextcloud 等现代 WebDAV 服务也能正常握手。
  副作用：公司网络里做 TLS 中间人（自签 CA）时证书不会被信任，同步会失败。
- 建议安装所有 Win7 最终补丁并保持杀毒软件病毒库更新；Win7 已 EOL，网络风险自负。

## 安全提醒

- 发布产物**没有代码签名证书**：首次运行可能出现 SmartScreen “Windows 已保护你的电脑”
  提示，需要“更多信息 → 仍要运行”；某些杀毒软件可能对“全局键盘钩子 + 模拟按键”行为
  误报，请从可信来源下载并核对 SHA256。
- 校验下载：发布目录外的 `dist/SHA256SUMS` 记录了 exe 的 SHA-256。Linux/macOS 上：

  ~~~
  cd dist
  sha256sum -c SHA256SUMS
  ~~~

  Windows PowerShell 上：

  ~~~
  Get-FileHash .\vibekey.exe -Algorithm SHA256
  ~~~

  把结果与 `SHA256SUMS` 中对应行比对。
- 程序为了拦截 Alt 组合键会安装全局低级键盘钩子（WH_KEYBOARD_LL）。它只处理自己的
  绑定表、其余按键原样透传，也不记录按键内容；但这类行为天然敏感，请只用可信构建。
- WebDAV 密码用 Windows DPAPI 加密保存在 `%APPDATA%\Vibekey\secret.bin`，
  只有当前用户能解密；密码不会出现在 config.toml、日志或同步文件中。
  换机器/重装系统需要重新输入密码。
- 程序默认不联网，只有启用 WebDAV 同步后才会访问你配置的 URL。
- 不要以管理员身份运行：程序以普通用户权限运行才能把文字注入普通窗口；
  对管理员权限的窗口注入会被 Windows UIPI 阻止（这是系统限制，不是 bug）。

## 常见问题

| 问题 | 处理 |
| --- | --- |
| 链接时报 `unable to find library -lgcc` | 没跑 `scripts/install-toolchain.sh`，或 sysroot 里的兼容符号链接被删了；重跑安装脚本 |
| 提示-Z 只能在 nightly 使用 | 用了 stable；构建命令必须带 `+nightly`（`scripts/build-win.sh` 已处理） |
| 提示 can't find crate for std / rust-src 缺失 | `rustup component add rust-src --toolchain nightly` |
| 提示 can't find crate for std（target may not support the target） | 忘记 `-Z build-std=std,panic_abort`，或 target 名拼错 |
| 程序在 Win7 启动即失败 | 用 `scripts/verify-win7-pe.sh` 检查子系统版本和导入表；确认是 win7 产物而不是普通 MSVC 构建 |
| 程序在 Win10 能跑、Win7 不能 | 同上，几乎都是拿了 `*-pc-windows-*` 的产物 |
| 提示缺少 `api-ms-win-crt-runtime-l1-1-0.dll` | 产物错误地依赖 UCRT；重新用本项目的脚本构建 |
| 杀毒软件报毒/删除 exe | 全局键盘钩子 + SendInput 的常见误报；从源码自行构建或核对 SHA256 后加白名单（风险自负） |
