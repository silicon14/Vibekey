# WebDAV 同步说明

Vibekey 可以把配置文件 `config.toml` 通过标准 WebDAV 同步到自己的网盘/服务器，
多台 Windows 机器之间共用同一份片段配置。

本文包括：

- 常见 WebDAV 服务的 URL 写法与认证方式；
- 同步语义（首次同步、本地变、远端变、双方都变）；
- 冲突备份文件命名；
- 为什么两类密码不会跟着配置一起同步；
- AES-256-GCM 内容加密、密码轮换与 HTTP 降级防护；
- 在本机自测同步的方法；
- 常见 HTTP 错误的含义与处理。

相关代码：

- `crates/vibekey-core/src/sync/engine.rs`：同步/冲突决策逻辑；
- `crates/vibekey-core/src/crypto.rs`：版本化加密信封、PBKDF2、AES-GCM 与篡改检测；
- `crates/vibekey-core/src/sync/webdav.rs`：基于 ureq 的 WebDAV 客户端；
- `crates/vibekey-core/src/sync/run.rs`：应用调用的高层入口 `sync_once`；
- `crates/vibekey-core/tests/webdav_integration.rs`：内嵌 WebDAV mock 的端到端测试。

## 一、配置项

在 `config.toml` 的 `[sync]` 段里配置（也可以在“设置与片段管理 → 同步与备份”里填写）：

    [sync]
    enabled = true
    url = "https://dav.example.com/vibekey/config.toml"
    username = "me"
    interval_minutes = 15
    sync_on_change = true
    backup_on_conflict = true
    encrypt = true
    timeout_secs = 20

| 字段 | 默认值 | 说明 |
| --- | --- | --- |
| `enabled` | `false` | 是否启用同步。关闭时不发起任何网络请求。 |
| `url` | 空 | **配置文件的完整 URL**（不是目录）。必须以 `http://` 或 `https://` 开头。 |
| `username` | 空 | WebDAV 用户名。留空则不做认证（仅适合内网匿名服务）。 |
| `interval_minutes` | `15` | 定时同步间隔；`0` 表示不做定时同步，只在手动/保存配置后同步。 |
| `sync_on_change` | `true` | 保存配置后立即同步一次。 |
| `backup_on_conflict` | `true` | 双方都修改时，先把远端内容备份到备份文件再覆盖；加密开启后备份也为密文。 |
| `encrypt` | `false` | 是否用独立同步密码加密认证远端配置。旧配置缺字段时保持兼容的明文模式。 |
| `timeout_secs` | `20` | 连接/读/写超时（秒），必须大于 0。 |

WebDAV 登录密码和配置加密密码**都不在** `config.toml` 里，见下文“密码与安全”。剪贴板历史仅保存在本机内存中，也不在此文件的同步范围内；退出后历史即清空。

## 二、常见 WebDAV 服务的 URL 写法

所有服务的 `url` 都要指向**最终的那个配置文件**，父目录不存在时客户端会自动用
`MKCOL` 逐级创建（最多向上递归 8 层）。

### Nextcloud / ownCloud

1. 在网页端：右上角头像 → **设置** → **安全** → **应用密码**，创建一个应用密码
   （不要把登录密码直接填给第三方程序）。
2. URL 形如：

       https://cloud.example.com/remote.php/dav/files/<用户名>/Vibekey/config.toml

   其中 `<用户名>` 是 Nextcloud 登录名；`Vibekey` 目录不存在也没关系，会自建。
   如果使用 Nextcloud 的“文件访问控制/WebDAV 根前缀”插件，路径可能不是
   `/remote.php/dav/files/...`，以服务商文档为准。
3. 用户名填 Nextcloud 用户名，密码填**应用密码**。

### 坚果云

1. 登录坚果云网页端 → **账户信息** → **安全选项** → **添加应用密码**，
   生成一个只用于 WebDAV 的密码（坚果云要求第三方客户端使用应用密码）。
2. URL 形如：

       https://dav.jianguoyun.com/dav/Vibekey/config.toml

   注意路径前缀是 `/dav/`，后面跟网盘里的目录和文件名。
3. 用户名填坚果云**注册邮箱**，密码填应用密码。

### 自建 WebDAV（Apache / nginx / rclone / NAS）

只要服务端支持标准的 `GET` / `PUT` / `MKCOL` 和 HTTP Basic 认证即可，例如：

- Apache `mod_dav` + `AuthType Basic`；
- nginx 的 WebDAV 模块（需要 `dav_methods PUT MKCOL` 等配置，社区版 nginx 需要第三方模块）；
- rclone：`rclone serve webdav --addr :8080 --user me --pass secret /srv/vibekey`；
- 群晖/威联通等 NAS 的 WebDAV Server 套件（开启后通常地址为
  `https://nas.example.com:5006/共享目录/Vibekey/config.toml`，端口以套件设置为准）。

示例（rclone，本机自测）：

    url = "http://127.0.0.1:8080/vibekey/config.toml"
    username = "me"

### 认证方式

客户端只实现 **HTTP Basic 认证**（`Authorization: Basic base64(用户名:密码)`）。
用户名非空时，客户端会给每个请求预先加上 Basic 头；服务器返回
`401 Unauthorized`（标准实现应同时带 `WWW-Authenticate: Basic realm="..."` 挑战）
即判定认证失败，不会自动重试。由于用户名/密码只是 Base64 编码后随请求头发送，
**正式使用必须走 HTTPS**。Digest/OAuth/令牌认证暂不支持。`sync.encrypt` 只加密配置正文，不会加密 HTTP 请求头，因此不能隐藏 Basic 用户名/密码，也不能替代 HTTPS。

## 三、同步语义

同步状态保存在配置文件旁边的 `sync-state.json`，记录本地/远端**解密后明文**内容 hash、
远端格式（`remote_encrypted`）、ETag 和上次结果。是否“变了”以明文 SHA-256 判断，随机密文不会造成假变化；
不依赖服务器一定返回 ETag——ETag 只用于条件上传（`If-Match`）。格式迁移与密码轮换独立触发上传，
只切换 `sync.encrypt` 不会被当成真实远端编辑。已确认加密的远端若变成明文，在仍启用加密时会返回 `Downgrade` 并停止所有写入。

每次同步（手动、定时、保存配置后）按以下顺序决策：

### 1. 首次同步（本机没有 `sync-state.json`）

- 远端**没有** `url` 指向的文件：上传本地配置（`Pushed { first_upload: true }`），
  并自动创建缺失的父目录。
- 远端**已有**文件：以远端为准拉取，覆盖本地（`Pulled`），旧本地文件备份为
  `config.toml.bak`。这正是“新机器接管已有配置”的场景。新机器要先在本地设置加密开关和同一密码；
  拉取不会根据远端内容擅自关闭本地加密策略。
- 本地刚启用加密而远端仍是旧明文：先验证远端 TOML，保留本地 `encrypt=true`，再把同一远端内容加密上传并写回本地；
  下次同步才是普通 no-op。上传失败不会留下未提示的本地重载。

### 2. 只有本地变化

本地 hash 与状态里记录的不同、远端内容没变：用 `If-Match: <上次 ETag>` 上传
（`Pushed`）。如果上传时服务器返回 `412`（说明刚好有别的客户端改过），客户端
会重新 `GET`：

- 远端内容其实没变、只是 ETag 变了：按新 ETag 再上传一次（仍然 `Pushed`）；
- 远端内容确实变了：进入“双方都变”的冲突处理。

### 3. 只有远端变化

本地没变、远端 hash 变了：下载远端内容并覆盖本地（`Pulled`），旧本地文件备份为
`config.toml.bak`。下载后会先校验它是合法的 TOML 配置；解析失败则**拒绝覆盖**
本地（`RemoteInvalid` 错误），避免坏文件把本机配置冲掉。

### 4. 两边都变了

采用**本地优先**（不自动合并）：

1. 若 `backup_on_conflict = true`，先把远端内容上传到备份文件
   （`ConflictLocalWins { remote_backup: Some(url) }`）；
2. 再用本地内容覆盖远端（`ConflictLocalWins`）。

配置冲突后请人工比较备份文件与本地配置，把需要的远端改动补回本地。启用加密时，主文件和备份都先解密再参与冲突判断，
备份也使用当前新密码重新加密；任何解密/认证失败都会停止，不会用本地版本强盖无法读取的远端。

### 5. 其它情况

- 两边内容完全相同：`UpToDate`，只刷新状态；
- 远端文件被删除：用本地文件重新创建远端（`Pushed { first_upload: false }`）。

## 四、备份文件命名

假设远端配置 URL 是：

    https://dav.example.com/vibekey/config.toml

双方都改时，远端原始内容会被保存为（时间戳是本地时间，格式 `YYYYmmdd-HHMMSS`）：

    https://dav.example.com/vibekey/config.toml.remote-backup-20261106-110000.toml

即：**原远端文件名 + `.remote-backup-<时间戳>.toml`**，与配置文件在同一目录。

本地这一侧被远端覆盖前，旧文件会保存为：

    %APPDATA%\Vibekey\config.toml.bak

（Linux 测试时为 `$XDG_CONFIG_HOME/vibekey/config.toml.bak`。）

备份文件不会被自动清理，需要时请手动删除。未启用加密时远端备份是普通 TOML；启用加密后备份与主文件一样为二进制密文，
需由装有同一同步密码的 Vibekey 解密，不能直接用文本编辑器查看。本地 `config.toml.bak` 始终是明文，请注意本机文件权限。

## 五、密码与安全

### 两类密码与本地存储

- **WebDAV 登录密码**用于 HTTP Basic，**独立配置加密密码**用于加密正文；两者用途和输入框分开。
- Windows 上分别用不同 DPAPI entropy 加密到 `%APPDATA%\Vibekey\secret.bin` 和 `sync-encryption.bin`，
  仅当前 Windows 用户/机器可解密；均不写入 TOML 或日志。
- 新机器要分别重新输入。配置加密密码至少 8 个字符，建议使用足够长、唯一的密码短语。
- 推荐给 WebDAV 单独创建**应用密码**（Nextcloud/坚果云都支持），怀疑泄露时到服务端撤销。
- 需要移除凭据时分别点“清除登录密码”或“清除加密密码”再应用。清除加密密码后无法再解开已有密文。

### 加密格式与密码轮换

- 版本 1 信封由 `VIBEKEY\x01`、PBKDF2 参数、16 字节随机 salt、12 字节随机 nonce、密文和 GCM tag 组成。
- 每次上传通过操作系统 CSPRNG 生成新 salt/nonce，以 PBKDF2-HMAC-SHA256 **600,000 次**派生 AES-256 密钥；
  AES-256-GCM 同时认证头部和配置正文。头部参数有上限，错误密码、篡改、截断和未知版本均失败关闭。
- 更换密码时，当前新密码与上一密码在同一个 DPAPI blob 内原子保存。解密可先尝试新密码、再尝试旧密码，
  上传始终只用新密码；旧密码仅在新密文 PUT 成功且 `sync-state.json` 保存成功后删除。DPAPI 的完整读校验写事务由互斥锁串行化，
  条件提交避免旧任务覆盖新状态；一次轮换尚未确认时禁止再次换成第三个密码，先成功同步即可继续更换，防止崩溃边界丢失可用密码。
- 关闭加密属于显式迁移：只有本机 `remote_encrypted=true` 已确认加密基线后，用户主动关闭才允许继续；仍需已保存密码先解密验证远端，再上传明文。
  新机器或历史明文机器即使保存过密码，也不能在默认 `encrypt=false` 下把别人的密文自动覆盖成明文；必须先启用加密并成功同步。
- 忘记密码无法从密文恢复；本地 `config.toml` 和 `.bak` 仍是明文。若要重设远端，请先自行备份旧密文。

### 保护范围与限制

- 加密防止正文被直接读取，并通过 GCM 检测修改；`remote_encrypted` 还会阻止已加密远端被静默替换成明文。
- **HTTP 仍暴露 Basic 登录凭据、URL、文件大小和时间等元数据，正式使用必须 HTTPS。** 内容加密不是传输层安全替代品。
- AES-GCM 不防止拥有写权限的服务器/攻击者回滚到一份旧的合法密文；当前格式未实现单调代数或防重放账本。
- 本地配置和本地备份不加密；`sync-state.json`、`secret.bin`、`sync-encryption.bin`、`*.bak` 都不要提交到公开仓库。

## 六、本机测试方法

### 1. 自动测试（推荐，不依赖外网）

    cargo test -p vibekey-core

其中 `crates/vibekey-core/tests/webdav_integration.rs` 会在进程内用
`tiny_http` 起一个 mock WebDAV（随机本地端口），覆盖：

- 首次上传、父目录链 `MKCOL`（201/405/409）；
- 新机器拉取远端配置；
- 两台机器同时修改后的冲突与远端备份 URL；
- 过期 ETag 触发 `412` 后自动重试；
- 错误密码 → `401` 错误映射、`WWW-Authenticate` 挑战；
- 约 280KB 中文正文的字节级往返完整性；
- HTTP 下密文不含可搜索片段、随机密文二次同步无 PUT、新机器解密且保留本地策略；
- 错误密码、密文篡改、加密后明文 downgrade 全部失败关闭；
- 旧明文迁移、加密冲突备份、新/旧密码轮换和显式关闭加密；
- 加密 412 fresh GET 解密重试，以及同明文随机重加密后的 UpToDate/no-extra-PUT。

测试只访问 `127.0.0.1`，不会连接任何外部服务器。

### 2. 手工联调

本机起一个带 Basic 认证的 WebDAV 服务，例如：

    # 方式 A：rclone（任选其一）
    rclone serve webdav --addr 127.0.0.1:8080 --user me --pass secret /tmp/vibekey-dav

    # 方式 B：Docker（bytemark/webdav 镜像）
    mkdir -p /tmp/vibekey-dav
    docker run --rm -p 8080:80 \
        -e AUTH_TYPE=Basic -e USERNAME=me -e PASSWORD=secret \
        -v /tmp/vibekey-dav:/var/lib/dav \
        bytemark/webdav

然后临时把配置改成：

    [sync]
    enabled = true
    url = "http://127.0.0.1:8080/vibekey/config.toml"
    username = "me"
    interval_minutes = 0
    encrypt = true
    timeout_secs = 10

在设置窗口输入 WebDAV 密码 `secret` 和另一个至少 8 字符的配置加密密码，点“测试连接”，再“立即同步”。
用 `ls /tmp/vibekey-dav` 观察 `vibekey/config.toml` 是否出现；直接读取该文件应看到 `VIBEKEY` 开头的二进制信封，
而不是可搜索的 TOML 片段。第二台机器也必须先填写同一加密密码。

> 本地 `http://` 只用于联调，正式使用请改回 `https://`。

### 3. 观察同步状态

- `sync-state.json`：`last_outcome` 为 `pushed` / `pulled` / `up-to-date` /
  `conflict-local-wins`，`remote_encrypted` 记录已验证的远端格式，`last_error` 保存最近一次失败原因；
- `config.toml.bak`：最近一次被覆盖前的本地配置；
- 远端 `*.remote-backup-<时间戳>.toml`：冲突时被覆盖前的远端配置。

## 七、常见错误

| 错误 | 含义 / 常见原因 | 处理 |
| --- | --- | --- |
| `401 Unauthorized` | 用户名或密码错误；应用密码被撤销/未启用；服务器要求认证但没收到密码。服务器应返回 `WWW-Authenticate: Basic` 挑战。 | 在“同步与备份”里重新输入密码（优先应用密码）；确认服务端已开启 WebDAV。 |
| `403 Forbidden` | 认证通过但没有权限：路径只读、WebDAV 功能未开启、网盘配额满、服务器禁止 `MKCOL`/`PUT`。 | 换有写权限的目录/账号；确认服务端允许 WebDAV 上传。 |
| `404 Not Found` | `url` 路径写错，或远端文件还不存在。**首次同步时 GET 返回 404 是正常的**，随后客户端会直接上传。 | 检查 `url` 是否指向文件（而不是目录）、大小写和路径前缀（如 `/remote.php/dav/files/`、`/dav/`）。 |
| `409 Conflict` | `MKCOL` 找不到父目录（客户端会自动逐级向上创建）；也可能是 URL 层级过深（超过 8 层）或服务器拒绝建目录。 | 检查 URL 中是否有多余字符/特殊字符；提前在网盘里手动建好父目录。 |
| `412 Precondition Failed` | 条件上传 `If-Match` 的 ETag 与服务器当前值不一致：其他设备刚改过远端文件。 | 通常无需处理：客户端会自动重试，若远端内容确实变了则按“本地优先”走冲突备份。持续出现说明多台机器在反复同时保存。 |
| `405 Method Not Allowed` | 只出现在 `MKCOL`：目录已经存在。客户端把它当作成功，不需要处理。 | 无。 |
| 网络/超时错误 | 域名解析失败、代理、防火墙、证书问题（Win7 上由内置根证书校验，过期证书会报错），或 `timeout_secs` 太小。 | 确认能访问 `https://...`，适当增大 `timeout_secs`，检查代理/DNS。 |
| 远端配置无效 | 远端文件不是合法 TOML 配置（被别的程序改写/截断）。 | 修正远端文件；客户端不会用坏文件覆盖本地配置。 |
| 配置加密密码错误 / 密文被修改 | AES-GCM 无法区分错密码与篡改，二者都拒绝解密。 | 核对独立同步密码及远端来源；不会覆盖本地，也不会强盖远端。 |
| 加密格式无效 / 不支持 | 信封被截断、参数越界或来自未来版本。 | 保留原文件并排查；不要重命名成 TOML 强行导入。 |
| 远端从加密变成明文 | 可能是 HTTP 降级或服务器被替换；`remote_encrypted` 检测到格式倒退。 | 检查服务器与网络；程序已停止读写，不要直接关闭加密绕过。 |
| 忘记配置加密密码 | 密码不随配置同步，无法从密文恢复。 | 使用仍存有密码的设备；或用本地明文配置重新建立远端，先备份旧密文。 |

错误会写入日志（`%APPDATA%\Vibekey\vibekey.log`），并在托盘气泡/设置窗口中提示。
