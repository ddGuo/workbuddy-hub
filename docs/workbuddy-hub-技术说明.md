# workbuddy-hub 技术说明

把多个 WorkBuddy 账号的任务收拢到一个列表，把切换账号变成一次点击，并在被限流时自动换号。

---

## 一、模型请求是谁发起的

**WorkBuddy 客户端自己。** hub 从不直接调用任何模型 API。

它做的只有一件事：把某个账号的凭据写回客户端的凭据文件，然后让客户端自己登录。之后所有的模型请求、上下文组装、流式响应，全部由官方客户端完成。

（每日签到是唯一的例外 —— 它和模型请求无关，只调签到接口，见第九节。）

请求里的 header、UA、设备指纹、请求签名，都是客户端原生的。服务端看到的是一次正常的账号登录和正常的对话请求。

### 与 DSH 插件的本质区别

| | DSH 插件 | workbuddy-hub |
|---|---|---|
| 谁发请求 | 插件自己直接调 API | WorkBuddy 客户端自己 |
| 身份特征 | 缺 header、UA 不同、无签名 | 原生 header、原生指纹、原生签名 |
| 服务端看到 | 一个可疑的非官方客户端 | 一个正常的官方客户端 |
| 结果 | 被识别、被拒（12153 等） | 无法区分 |

DSH 被拒，是因为它在**冒充**客户端，而服务端校验的正是"你是不是那个客户端"。hub 不冒充任何东西 —— 它让真客户端换了个账号登录。

### 实证

2026-10-22 09:45 切换到 `8bc5fd09`，09:47 起的新对话正常工作。切号后的模型请求没有任何异常。

---

## 二、切号具体改了什么

三个动作，顺序不能颠倒：

1. **退出客户端**（`taskkill` 不带 `/F`，发 WM_CLOSE 走正常退出）
2. **写凭据** —— 用客户端自己的 at-rest 密钥（AES-256-GCM）重新封装目标账号的两个 token，原子写入 `workbuddy-desktop.info`
3. **归集任务** —— 把 `sessions.user_id` 统一改成目标账号，让客户端列表显示全部任务
4. **启动客户端** —— 拉起后等进程真正出现

凭据文件和数据库都会先做备份，任一步失败自动回滚。

---

## 三、仍然存在的风险

不会出现"请求被拒绝"，但另一种情况存在 —— **风控**。

| 风险 | 表现 | 说明 |
|---|---|---|
| 设备维度异常登录 | 要求短信/扫码验证 | 同设备短时间内频繁换号 |
| 首次登录 | 额外验证 | 凭据文件里有 `account.isFirstLogin` 字段，客户端会区分 |
| 账号本身状态异常 | 切过去也不可用 | 与 hub 无关 |

### 应对

- **不要秒级连切**。冷却表按 10 分钟 / 30 分钟 / 24 小时三档退避。
- **老账号优先**。在这台设备登录过的账号，切过去最平滑。
- 切完等客户端完整加载再操作，不要立刻发消息。

---

## 四、已经实测验证的

| 能力 | 验证方式 | 结果 |
|---|---|---|
| 跨账号任务聚合 | 读 9 个账号的 SQLite | 13 个任务全部列出 |
| 凭据加解密 | 与客户端 Node 运行时双向对拍 | keyId / AAD / token 长度 / SHA-256 完全一致 |
| 批量解密 | 9 个账号逐个解密 | 9/9 成功 |
| 任务归集 | 真实执行 `unify --force` | 13 个任务合并到 `8bc5fd09`，归属表 19 条 |
| 切号 | 真实执行 | 账号从 `db0427fd` 变为 `8bc5fd09`，客户端正常工作 |
| 限流检测 | 扫真实错误会话 | 识别 `429`、判定模型级/账号级、解析重置时间 |
| 会话正文解析 | 真实 5 MB 会话（1483 条记录） | 30 问 / 138 答 / 428 次工具调用，统计与上限裁剪都正确 |
| 签到状态查询 | 9 个账号真实调用 | 全部返回活动状态、连签天数、今日积分 |

---

## 五、还没验证的

| 事项 | 为什么没做 | 风险 |
|---|---|---|
| 归集后客户端是否清理"非自己"的会话 | 静态分析看不到这个行为 | 中。首次归集建议在无任务时做，异常可 `--rollback` |
| `accounts` / `allAccounts` 多账号能力 | 刚发现，未深挖 | 低。若可用，能省掉重启 |
| `watch --auto` 自动换号 | 会重启客户端、中断当前任务 | 低。逻辑已就绪，只差真实触发 |
| 改完 `user_id` 免重启刷新列表 | 客户端不监听数据库变更，只能靠失焦/聚焦或重启 | 低。界面限流提示里只给建议，不自动切 |
| 签到领取（`daily-checkin`） | 当天 9 个账号都已在客户端签过，领取分支没有真实触发 | 低。失败会把服务端原文写进记录并显示 |
| L2 MITM 代理（真无感切换） | 需要装根证书，是安全决策 | 高。未采纳 |

---

## 六、环境限制（本机特有）

### 1. 从被注入的终端启动会黑屏

从某些被注入了沙箱组件的终端启动，WebView2 子进程会被注入第三方 DLL（`tsbx.dll`），渲染进程崩溃，窗口全黑。

判断：

```powershell
Get-ChildItem "$env:LOCALAPPDATA\cn.workbuddy.hub" -Recurse -Filter *.dmp |
  Sort-Object LastWriteTime -Descending | Select-Object -First 1
```

每次启动都新增 `.dmp` 就是这个问题。注入沿**进程树**传播，清环境变量无效 —— 必须从桌面或开始菜单启动。

### 2. 控制台闪窗（已修）

切号要反复探测客户端是否退出，早期实现每 300ms 调一次 `tasklist`，20 秒超时最多闪 66 次黑框。

现在所有外部命令都通过 `proc::hidden()` 构造，带 `CREATE_NO_WINDOW`，轮询降为 500ms / 15 秒。界面上显示分步进度，不再是空白等待。

---

## 七、命令速查

```bash
hub probe                  # 探测客户端、凭据、数据目录
hub accounts               # 账号列表与凭据到期
hub tasks                  # 跨账号任务聚合
hub verify                 # 逐个解密账号凭据
hub switch --to <uid>      # 切号（默认演练，--force 执行）
hub unify                  # 任务归集（默认演练）
hub unify --status         # 归集状态
hub unify --rollback       # 按原始归属恢复
hub watch --once           # 扫一轮限流信号
hub watch --auto           # 持续监控并自动换号
hub transcript <id>        # 预览会话正文（--tail N 控制条数）
hub checkin --status       # 只看签到状态，不领取
hub checkin                # 逐账号签到
```

界面版（Tauri）提供同样的能力，切号与归集都带二次确认和分步进度：

| 界面能力 | 说明 |
|---|---|
| 任务详情抽屉 | 点任务卡片预览对话；工具调用一行、点开看输出；正文只读 |
| 限流提示条 | 每分钟扫一次失败会话，显示账号 / 范围 / 冷却时间 / 原文，可一键换号 |
| 托盘常驻 | 关窗只隐藏，托盘菜单「显示主窗口 / 退出」，左键单击拉回前台 |
| 单实例 | 第二个进程通过 `127.0.0.1:47821` 叫出已有窗口后退出 |
| 主题 | 跟随系统 / 浅色 / 深色；`data-theme` + `prefers-color-scheme` 监听 |
| 无边框标题栏 | 隐藏系统标题栏，顶部一行 `data-tauri-drag-region` 可拖动、双击最大化；窗口按钮在主题切换之后 |
| 每日签到 | 侧栏卡片显示进度与连签天数，账号行标记成败，手动 / 自动两种触发 |
| 开机自启 | 勾选后写 `HKCU\...\Run\WorkBuddyHub`，卸载时手动删该项即可 |

---

## 八、数据与安全边界

- 凭据、数据库、原始归属表都只在本机读写，不上传任何地方。
- at-rest 密钥通过客户端自己的 Node 运行时取出，不做持久化。
- 归集会改写 `sessions.user_id`，但保留原始归属表，随时可回滚。
- 所有写操作前自动备份：凭据 `.info`、数据库用 SQLite 在线备份（含 WAL）。
- 签到是唯一外发请求的功能：token 只发往账号自己的 domain，不经过第三方，
  不写日志、不落临时文件（curl 配置走 stdin）。

---

## 九、每日签到

客户端里对应的实现是 `CloudAccountRepo.checkinStatus()` / `checkin()`：

```
POST {endpoint}/v2/billing/meter/checkin-activity-status
POST {endpoint}/v2/billing/meter/daily-checkin
```

鉴权复用账号自己的凭据：

| 头 | 来源 |
|---|---|
| `Authorization: Bearer <accessToken>` | 账号库解密 |
| `X-User-Id` | 账号 uid |
| `X-Domain` | 账号 domain（`www.workbuddy.cn`） |
| `X-Device-Token` | 客户端 Turing SDK 生成，**hub 不伪造** |

实测缺 `X-Device-Token` 也能正常查状态，说明它不是硬门槛；万一服务端某天要求它，
返回码会原样落到记录里，界面能看到具体原因。

### 实现选择

| 选择 | 理由 |
|---|---|
| 用系统 `curl`，不引 HTTP 库 | 少几十个依赖；curl 走 Schannel，复用系统信任链 |
| 配置写 stdin（`curl -K -`） | token 不进 argv、不落盘，进程列表里看不到 |
| 启动 20 秒后跑，之后每 30 分钟 | 开机即签，跨 0 点也能补；当天已有结论的账号跳过 |
| 失败账号 6 小时内不重试 | 凭据失效的账号不该被反复请求；手动签到不受限制 |
| 记录落 `~/.workbuddy-hub/checkin.json` | 界面展示连签/成败；也能防止重复领取 |

### 返回码

| 情况 | 记录状态 | 界面文案 |
|---|---|---|
| 领取成功 | `claimed` | 签到成功 · +N 积分 |
| 今天已经签过 | `already` | 今天已签到 · 连签 N 天 |
| 活动没开 | `inactive` | 签到活动未开启 |
| 网络 / 鉴权 / 服务端拒绝 | `error` | 具体原因（HTTP 码 + 服务端 msg） |

---

## 十、客户端数据结构与凭据格式

### 路径

| 用途 | 路径 |
|---|---|
| 会话索引 | `~/.workbuddy/workbuddy.db` → `sessions` 表（`user_id` 为过滤键） |
| 会话正文 | `~/.workbuddy/projects/<编码后的 cwd>/<session-id>.jsonl` |
| 附件 | `~/.workbuddy/blobs/` |
| 凭据 | `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info` |
| 主密钥槽 | `~/.workbuddy/keyblob` |
| 账号库（复用） | `~/.dsh/workbuddy-accounts/<uid>.json` |
| hub 自己的状态 | `~/.workbuddy-hub/`（归属表、冷却表、签到记录、备份） |

### 凭据文件

```json
{
  "auth": {
    "accessToken": { "$wbEncrypted": 1, "envelope": "<base64>" },
    "refreshToken": { "$wbEncrypted": 1, "envelope": "<base64>" },
    "expiresAt": 1794897148233,
    "domain": "www.workbuddy.cn"
  },
  "account": { "uid": "...", "nickname": { "$wbEncrypted": 1, "envelope": "..." } },
  "accounts": [ ... ],
  "allAccounts": [ ... ]
}
```

envelope 解码后是 `{suite, keyId, nonce, authTag, ciphertext}`。

### 加解密

```
key    = SHA256(atRestSecretKey)
keyId  = hex(SHA256(key))[..16]
cipher = AES-256-GCM
AAD    = "WB-AAD\0" ‖ 0x01 ‖ LP("WBEV1") ‖ LP("sym-v1") ‖ suite(u32be)
                    ‖ LP(keyId) ‖ 0x02 ‖ 0x00 ‖ 0x00
LP(x)  = u32be(len) ‖ bytes
```

`atRestSecretKey` 只能让客户端自己给：

```bash
# ELECTRON_RUN_AS_NODE=1
WorkBuddy.exe -e "process.stdout.write(String(
  process._linkedBinding('electron_browser_workbuddy_storage').loggerGet()))"
```

### 会话 jsonl 的记录类型

`session-meta` / `ai-title` / `message` / `reasoning` / `function_call` /
`function_call_result` / `file-history-snapshot`。任务详情只展示后四种中的人可见部分，
`reasoning` 只统计字数。

---

## 十一、M1 对拍怎么做

解密逻辑是从 `dsh-workbuddy-switcher` 的 `importCurrent()` 移植过来的，
唯一风险是 AAD 拼装或长度前缀写错 —— 这类错误不会「部分成功」，只会整体解不开。
所以两边各解一次、比对哈希即可：

```bash
# TypeScript 实现（用客户端自带的 Node 运行时，才有 linked binding）
ELECTRON_RUN_AS_NODE=1 "D:/WorkBuddy/WorkBuddy.exe" tools/parity.cjs

# Rust 实现
hub authhash

# 两行的 accessSha256 / refreshSha256 必须完全一致
```

只比哈希，不打印明文 —— 对拍只需要确认一致，不需要把 token 落到终端里。

---

## 十二、账号资料（昵称 / 手机号）

本地只有**当前登录账号**的资料：客户端凭据文件里的 `account.nickname` 和
`account.phoneNumber` 都是加密字段，能用同一把 at-rest 密钥解开；其余账号在
`~/.dsh/workbuddy-accounts/` 里只有 uid。

要拿到全部账号的手机号，用各账号自己的 token 打账号接口：

```
GET {endpoint}/console/accounts
Authorization: Bearer <accessToken> · X-User-Id · X-Domain
```

返回的 `data.accounts[]` 里有 `uid / uin / nickname / phoneNumber / email / type`。
客户端自己刷新账号列表（`observeAccountRefresh`）走的也是这个接口。

### 处理规则

| 规则 | 原因 |
|---|---|
| 手机号脱敏后再落盘（`199****9283`） | 界面只需要「认出是哪个号」，完整号码没必要保存和显示 |
| 缓存 7 天，`force` 才全部重拉 | 手机号几乎不变，9 个账号每次启动都请求没有意义 |
| 显示优先级：手机号 → 昵称 → uid 前缀 | 有些账号（如微信登录）没有手机号 |
| 缓存文件 `~/.workbuddy-hub/profiles.json` | 离线也能显示；不进入仓库，也不含完整号码 |

实测 9 个账号都返回了手机号；昵称多数就是脱敏手机号本身，两个自定义昵称
（`· Y ·`、`G`）的账号仍然优先显示手机号。
