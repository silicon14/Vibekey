# 配置参考（config.toml）

Vibekey 的全部配置都在一个纯文本 TOML 文件里。字段定义以
`crates/vibekey-core/src/config.rs` 为准，本文件逐字段说明类型、默认值和校验规则。

- 想直接复制一份能用的配置：见 `assets/config.example.toml`。
- WebDAV 同步的服务端设置：见 [webdav.md](webdav.md)。
- Windows 7 构建与 PE 校验：见 [win7.md](win7.md)。

## 配置文件位置

Windows 上配置文件位于应用数据目录：

~~~
%APPDATA%\Vibekey\config.toml
~~~

通常是 `C:\Users\<用户名>\AppData\Roaming\Vibekey\config.toml`。
首次启动时如果文件不存在，程序会自动写入一份带示例片段的默认配置；
配置文件损坏时程序会提示并继续使用默认配置，不会覆盖坏文件。

同一个目录下还有这些文件：

| 文件 | 说明 |
| --- | --- |
| `config.toml` | 本文件，手动编辑或由设置窗口保存 |
| `secret.bin` | 经 Windows DPAPI 加密的 WebDAV 密码，**不会**被同步 |
| `sync-state.json` | 同步状态（本地/远端内容 hash、ETag、上次同步时间），仅本机使用 |
| `sync-state.json.tmp` | 保存状态时的临时文件 |
| `config.toml.tmp` | 原子保存配置时的临时文件 |
| `config.toml.bak` | 从远端拉取配置、覆盖本地之前的备份 |
| `vibekey.log` / `vibekey.log.1` | 日志，超过 1 MB 时轮转一次 |

在 Linux 上跑单元测试时，目录是 `$XDG_CONFIG_HOME/vibekey`（未设置则 `~/.config/vibekey`）。

## 语法与默认值

- 文件必须是 UTF-8 编码的 TOML，支持 `#` 注释。
- **缺失的字段一律回退到默认值**（serde 的 `default` 行为），所以最小配置只需要写想改的字段。
- **未知字段会被静默忽略**，字段名拼错不会报错，只会“不生效”，修改时请对照本页。
- 枚举值必须完全匹配，且为小写：`activation` 只能是 `immediate` 或 `hold`，`input_method` 只能是 `paste` 或 `type`。
- TOML 的表格顺序有讲究：全局字段和 `[hotkeys]`、`[sync]` 要写在所有 `[[snippet]]` 之前，片段数组放在文件最后（否则后面的表会被解析成上一个片段的一部分）。

## 顶层字段

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `activation` | 字符串枚举 | `immediate` | 触发方式。`immediate`：按住 Alt 后按数字键立即触发；`hold`：Alt 必须先按住 `hold_ms` 毫秒，再按数字键才触发，用于减少误触。 |
| `hold_ms` | 整数 | `350` | `hold` 模式下 Alt 需要按住的毫秒数。建议范围 100–3000，超出只是警告。 |
| `input_method` | 字符串枚举 | `paste` | 文字注入方式。`paste`：写入剪贴板后发送 Ctrl+V（默认，中文/Unicode 最稳）；`type`：用 SendInput 逐字符输入，适合不允许改动剪贴板的程序。 |
| `restore_clipboard` | 布尔 | `true` | 粘贴后是否恢复剪贴板原有内容。为 `true` 时：纯文本会完整恢复；剪贴板里是文件/图片/富文本（HTML/RTF 等）时程序**不覆盖剪贴板**，自动改用逐字符输入。为 `false` 时直接用剪贴板粘贴，原内容会被片段覆盖。 |
| `start_with_windows` | 布尔 | `false` | 开机自启动。实际写入 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`，托盘菜单「开机启动」会同步这个字段。 |
| `ignore_apps` | 字符串数组 | `[]` | 在这些程序里不触发任何快捷键，按前台进程的 exe 文件名匹配，不区分大小写，例如 `["photoshop.exe"]`。空字符串条目会触发警告。 |
| `[hotkeys]` | 表 | 见下 | 内置菜单的快捷键。 |
| `[sync]` | 表 | 见下 | WebDAV 同步设置。 |
| `[[snippet]]` | 表数组 | 空 | 片段列表，数组顺序就是列表弹窗中的显示顺序。 |

## `[hotkeys]`：内置菜单快捷键

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `list` | 字符串 | Alt + 反引号 | 打开片段列表。默认值是 Alt 加反引号键（TOML 中也可写成 `alt+grave`），见下方语法示例。 |
| `config` | 字符串 | `alt+backspace` | 打开配置菜单。 |

两个值都不能为空；为空是校验错误。它们不能和任何 `[[snippet]]` 的 `key` 冲突。

### 快捷键语法

格式是「修饰键 + 的最后一项是按键」，用加号分隔，不区分大小写：

~~~
alt+1
alt+backspace
ctrl+shift+h
alt+`        # 反引号
alt+grave    # 反引号的别名
~~~

- 修饰键（至少一个）：`alt`（别名 `option`、`opt`）、`ctrl`（`control`）、`shift`、`win`（`super`、`meta`、`windows`）。
- 按键：单个可打印 ASCII 字符（如 `1`、`a`、`-`、`=`），或下列名字：
  - `backspace`（`bksp`、`bs`）、`tab`、`enter`（`return`）、`esc`（`escape`）、`space`（`spacebar`）
  - `delete`（`del`）、`insert`（`ins`）
  - `left` / `right` / `up` / `down`（也可写 `arrowleft` 等）
  - `home`、`end`、`pageup`（`pgup`）、`pagedown`（`pgdn`）
  - `grave` / `backtick` / `tilde`（反引号键）
  - `f1` 到 `f24`
- 必须至少有一个修饰键，单独 `1` 或 `h` 不是合法快捷键。
- 解析失败、未知按键、重复绑定都会产生**错误**，快捷键层会停止工作（程序本身仍运行）。

## `[[snippet]]`：片段

每个 `[[snippet]]` 表示一条可插入的文本：

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `key` | 字符串 | `""` | 直接快捷键的槽位：`1`–`9`、`0`、`-`、`=`，分别对应 Alt+1 … Alt+0、Alt+-、Alt+=。空字符串表示**只出现在列表里**、没有直接快捷键。 |
| `name` | 字符串 | `""` | 列表里显示的名称，可以留空。 |
| `text` | 字符串 | `""` | 要插入的文本，支持模板变量（见下）。 |
| `send_enter` | 布尔 | `false` | 插入文本后是否再发送一次回车，例如在聊天软件里直接发出。 |
| `enabled` | 布尔 | `true` | `false` 的片段会被完全忽略：既不出现在列表里，也不能用快捷键触发。 |

校验规则：

- `key` 必须是上面 12 个槽位之一或空字符串，其他值报错。
- 同一个 `key` 只能出现一次；和 `hotkeys.list` / `hotkeys.config` 冲突也报错。
- `enabled = false` 的片段不参与上述冲突检查。
- 没有 `key` 的片段（`key = ""`）不会和任何快捷键冲突。

## 模板变量

`text` 中可以使用这些变量：

| 变量 | 展开结果 |
| --- | --- |
| `{date}` | 本地日期，格式 `YYYY-MM-DD` |
| `{time}` | 本地时间，格式 `HH:MM` |
| `{datetime}` | 本地日期时间，格式 `YYYY-MM-DD HH:MM` |
| `{clipboard}` | 触发瞬间剪贴板中的文本（没有则空字符串） |
| `{user}` | Windows 用户名 |
| `{host}` | 计算机名 |

- 写 `{{date}}` 会输出字面量 `{date}`（双写花括号转义）。
- 未知变量（例如 `{foo}`）原样保留。
- 变量名前后允许空格，且不区分大小写（`{ DATE }` 等价于 `{date}`）。

## `[sync]`：WebDAV 同步

| 字段 | 类型 | 默认值 | 说明 |
| --- | --- | --- | --- |
| `enabled` | 布尔 | `false` | 是否启用同步。 |
| `url` | 字符串 | `""` | 远端配置文件的完整 URL（**指向文件**，不是目录），必须以 `http://` 或 `https://` 开头。父目录不存在时会自动创建（MKCOL）。 |
| `username` | 字符串 | `""` | WebDAV 用户名。为空时不发送认证头。 |
| `interval_minutes` | 整数 | `15` | 每隔多少分钟自动同步一次；`0` 表示不定时，只在手动或保存配置时同步（会产生警告）。 |
| `sync_on_change` | 布尔 | `true` | 保存配置后立即同步一次。 |
| `backup_on_conflict` | 布尔 | `true` | 两端都修改时，先把远端版本备份为 `<url>.remote-backup-<时间戳>.toml`，再用本地版本覆盖远端。 |
| `timeout_secs` | 整数 | `20` | 网络连接/读/写超时（秒），必须大于 0。 |

**密码不在这个文件里。** 密码通过设置窗口输入，Windows 上由 DPAPI 加密后保存在
`%APPDATA%\Vibekey\secret.bin`，只有当前 Windows 用户可以解密。因此：

- 同步出去的 `config.toml` 不含任何秘密，可以放心放进版本库。
- 换机器或重装系统后需要重新输入一次密码。
- 日志中不会出现明文密码。

同步的冲突策略、各服务商 URL 示例见 [webdav.md](webdav.md)。

## 校验规则汇总

程序加载配置时会做两层检查：TOML 解析（语法错误 → 提示并保留旧配置）和字段校验。
字段校验分「错误」与「警告」：

| 情况 | 级别 |
| --- | --- |
| `hotkeys.list` / `hotkeys.config` 为空 | 错误 |
| 快捷键解析失败（空、无修饰键、未知键） | 错误 |
| 两个片段使用同一个 `key`，或片段与菜单快捷键冲突 | 错误 |
| `snippet.key` 不是 12 个槽位之一 | 错误 |
| `sync.enabled = true` 但 `sync.url` 为空 | 错误 |
| `sync.enabled = true` 但 `sync.url` 不以 `http://` / `https://` 开头 | 错误 |
| `sync.enabled = true` 且 `sync.timeout_secs = 0` | 错误 |
| `hold_ms` 不在 100–3000 | 警告 |
| `sync.enabled = true` 且 `interval_minutes = 0`（不定时同步） | 警告 |
| `ignore_apps` 含空字符串 | 警告 |

出现错误时程序仍会启动：启动时 TOML 解析失败会退回默认配置并提示，运行中解析失败会保留
旧配置；快捷键表构建失败会暂停快捷键触发并在托盘气泡提示；警告只记录在日志里，不影响使用。

## 修改如何生效

- 程序会轮询配置文件的修改时间，检测到变化后自动重新加载。
- 也可以右键托盘图标 →「重新加载配置」。
- 设置窗口保存时会做原子替换（写临时文件再改名），不会留下半个文件。
- 用外部编辑器修改后建议检查日志里有没有校验警告。

## 最小配置示例

只改输入方式和两条片段：

~~~toml
input_method = "paste"
restore_clipboard = true

[[snippet]]
key = "1"
name = "问候"
text = "你好，我是 {user}。"

[[snippet]]
key = "2"
name = "时间"
text = "现在是 {datetime}"
send_enter = true
~~~

## 完整示例

完整、可直接复制的配置（包含同步和所有字段的中文注释）在
[assets/config.example.toml](../assets/config.example.toml)。下面是其中与快捷键相关的部分：

~~~toml
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
name = "打招呼"
text = "你好，我是 {user}，很高兴认识你。"
send_enter = false
enabled = true

[[snippet]]
key = ""
name = "剪贴板内容"
text = "{clipboard}"
enabled = true
~~~
