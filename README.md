# workbuddy-hub

**简体中文** | [English](README.en.md)

**[下载安装包](https://github.com/ddGuo/workbuddy-hub/releases/latest)**（Windows 10 1803+）

把散落在多个 WorkBuddy 账号里的任务收拢成一个列表，需要时一键切换客户端登录的账号；
顺带接管每天要手动点一遍的签到。

![深色界面](docs/ui-dark.png)

## 它解决什么

| 痛点 | hub 的做法 |
|---|---|
| 任务散在 9 个账号里，客户端一次只看得到一个 | 只读聚合全部账号的会话，按账号 / 时间分组 |
| 换号要退出、重新登录，换完还找不到原来的任务 | 退出 → 写凭据 → 归集 → 重启，一次点击；原始归属可回滚 |
| **换号后任务和对话都断了** | **切到新账号后所有任务仍在列表里，点开就是完整对话** |
| 被 429 限流只能自己发现、自己去换号 | 扫描错误会话识别 429，界面上给出一键换号建议 |
| 每天挨个账号点签到 | 启动后自动签，连签天数、失败原因都可见 |
| 想知道某个任务聊了什么，得去翻 jsonl | 点卡片直接看对话，工具调用可展开 |

### 换号不断档（核心亮点）

切号的目的通常是「用另一个账号的额度接着干活」，而不是「换个账号从零开始」。

hub 在切号时会把已有会话的归属改到新账号，于是：

- 客户端的**原生任务列表**直接显示全部任务，不需要在 hub 里另开一套
- 点开任意一条，还是原来那段对话 —— 消息、工具调用、文件快照都在，接着往下聊就行
- 会话正文（jsonl）**一个字节都不改**，只改数据库里的 `sessions.user_id`
- 原始归属单独存档，`hub unify --rollback` 随时恢复

实测：把 9 个账号的任务切到另一个账号后，任务列表完整、上下文连续。

## 功能一览

- **换号不断档**：切号后任务不丢、上下文跟着走，接着原对话继续
- **任务总览**：跨账号聚合，状态筛选、标题 / 路径 / 账号搜索（按 `/` 聚焦），按账号或时间分组
- **账号识别**：列表显示脱敏手机号（如 `199****9283`），没有手机号时回退昵称 / uid 前缀
- **一键切号**：备份凭据与数据库 → 原子写入 → 归集 → 重启客户端，失败自动回滚
- **任务归集**：改写 `sessions.user_id`，让客户端原生列表显示全部任务，原始归属单独存档
- **限流检测**：从会话正文里读 429 原文，识别范围（单模型 / 账号额度）与重置时间
- **任务详情**：点卡片看完整对话，工具调用合并成一行、点开看输出
- **每日签到**：多账号自动签到，记录连签天数与失败原因
- **常驻托盘**：关窗只隐藏，开机自启，重复启动只唤起已有窗口
- **双主题**：跟随系统 / 浅色 / 深色

## 设计前提与边界

**模型请求始终由官方客户端自己发出。** hub 只经手客户端自己的凭据文件和数据库：
把某个账号的凭据放回去、把任务归属聚到当前账号。请求里的 header、UA、设备指纹、
签名都是客户端原生的，服务端看到的是一次正常的账号登录。

**直接调接口的只有两处：每日签到和账号资料。** 两者都用账号自己的 token 调官方接口
（签到查状态 + 每天领一次积分；资料读昵称与手机号），不涉及模型请求，不经过任何第三方，
也不伪造客户端的反爬头。除此之外 hub 不向服务端发任何请求。

**只读聚合，写操作可回滚。** 任务列表按 `sessions.user_id` 过滤，所以归集会改写这个字段；
原始归属记在 `~/.workbuddy-hub/ownership.json`，随时 `hub unify --rollback` 恢复。

> 原理、风险边界与实测结论见 [技术说明](docs/workbuddy-hub-技术说明.md)。

---

## 快速开始

### 安装

从 [Releases](https://github.com/ddGuo/workbuddy-hub/releases/latest) 下载
`WorkBuddy.Hub_0.1.0_x64-setup.exe` 双击安装；也可以下 `workbuddy-hub.exe`
（免安装，前端已嵌进二进制）。

想直接发给别人，用这个固定链接（永远指向最新版）：

```
https://github.com/ddGuo/workbuddy-hub/releases/latest
```

> 安装包没有代码签名，Windows SmartScreen 可能拦一下：点「更多信息 → 仍要运行」。
> 介意的话可以从源码构建，见下文。

装完从**开始菜单或桌面快捷方式**启动。启动一次之后常驻托盘，
以后重复点图标只会把已有窗口叫到前台，不会开出第二个实例。

推送 `v*` 标签会自动构建并发布新版本（`.github/workflows/release.yml`），
不需要本地打包再手动上传。

### 环境要求

| 依赖 | 说明 |
|---|---|
| Windows 10 1803+ | 需要 WebView2 运行时与系统自带 `curl`（Win11 均自带） |
| Rust stable | 只用于构建 |
| Node 20+ | 前端构建（用 npm，不要用 pnpm） |
| WorkBuddy 客户端 | 装在任意路径，hub 会自己探测 |

### 从源码构建

```bash
npm install          # 前端依赖（pnpm 会拦 esbuild 的构建脚本）
npm run dev          # 只跑界面（浏览器，示例数据）
npm run tauri:dev    # 官方开发模式
npx tauri build      # 打包安装程序（前端嵌进二进制）
```

> **必须用 Tauri CLI 构建。** 直接 `cargo build` 的产物不会加载前端资源，
> 窗口会是纯黑 —— Tauri 在那种模式下期望你自己跑 dev server。

---

## 核心机制

### 切号

四个动作，顺序不能颠倒：

1. **退出客户端**（`taskkill` 不带 `/F`，发 WM_CLOSE 走正常退出）
2. **写凭据** —— 用客户端自己的 at-rest 密钥（AES-256-GCM）重新封装目标账号的 token，原子写入
3. **归集任务** —— 把 `sessions.user_id` 统一改成目标账号
4. **启动客户端** —— 拉起后等进程真正出现

凭据文件和数据库都会先备份，任一步失败自动回滚。客户端有单实例锁（`app/lockfile`），
所以不能热切换。

### 任务归集

客户端只按 `sessions.user_id` 过滤任务列表，把所有账号的会话归到这个字段，原生列表就显示全部。
实测前提支持这么做：

- 18 条会话全部 `transport='local'`，不上云，改写不会被服务端覆盖
- 无触发器、无视图、无外键引用 `sessions`
- 涉及 `user_id` 的 4 个索引都是 `WHERE transport='cloud'` 的部分索引

```bash
hub unify                 # 演练（默认，不改库）
hub unify --force         # 真正改写，写前自动做在线快照
hub unify --status        # 查看归集状态
hub unify --rollback      # 按原始归属表恢复
```

归集是幂等的：只对「归属表里还没有」的会话记录原始 uid，重复执行不会污染记录。
覆盖已软删除的会话，否则删掉的任务换个号又会冒出来。

归集只改 `sessions.user_id`，**不碰会话正文**，所以换号后对话上下文、工具调用记录
全都原样保留 —— 这也是「换号不断档」的实现基础。

### 限流检测

客户端不把错误写进数据库，只把任务 `status` 置成 `error`。真正的错误文本在
会话正文最后一条 assistant 消息里：

```
429 您的使用量已超出频率限制，将在 2026-10-19 09:56:23 UTC+8 重置，
    您也可以切换其他模型继续使用。
```

这句话同时给出三件事：

| 从文本里读到的 | 用途 |
|---|---|
| `429` / `频率限制` / `积分不足` | 是否该换号 |
| `您也可以切换其他模型` | 范围是**单模型**还是**整个账号** |
| `将在 … 重置` | 精确冷却到那一刻；没有就按 10 分钟 / 30 分钟 / 24 小时退避 |

```bash
hub watch --once          # 扫一轮，只报告
hub watch                 # 持续监控，只报告
hub watch --auto          # 发现限流就换号（有任务在跑时自动跳过）
```

两个安全阀：**有任务在跑就不切**（重启会全打断），**冷却表持久化**
（同一账号不会因同一条错误被反复切走）。

界面里只做「报告」：每分钟扫一遍失败会话，读到 429 就在列表上方给出账号、范围、
冷却时间和原文，旁边是「换到 xxxxxxxx」按钮，点了走正常切号确认流程。

### 任务详情

会话正文在 `projects/<编码后的 cwd>/<session-id>.jsonl`，一个会话混着七种记录。
预览只取四种（用户、助手、工具调用、工具结果），`reasoning` 只统计字数不展示 ——
它通常占一半以上体积，铺开来反而看不到人说了什么。

| 处理 | 原因 |
|---|---|
| 剥离 `<system-reminder>` | 每轮用户消息前挂着几百行环境上下文，留着会把真正的提问挤出屏幕 |
| 只返回最近 400 条 | 跑了几天的会话有上万条记录，界面不需要全量 |
| 单条正文截断 4000 字 | 避免一次渲染几 MB 文本卡住 WebView |
| 工具调用与结果合并一行 | 一次 Bash 否则要占两条高度 |

解析是纯只读，不写任何文件。也可以命令行看：

```bash
hub transcript abffe1ce --tail 8
```

### 每日签到

客户端里对应两个接口，hub 用账号自己的 token 调用：

```
POST {domain}/v2/billing/meter/checkin-activity-status   查活动状态
POST {domain}/v2/billing/meter/daily-checkin             领取当天积分
Authorization: Bearer <accessToken> · X-User-Id · X-Domain
```

| 设计 | 原因 |
|---|---|
| 不引 HTTP 库，用系统自带 `curl` | 少几十个依赖；curl 走 Schannel，用系统信任链 |
| 请求配置写进 `stdin`（`curl -K -`） | token 不出现在进程命令行，也不落临时文件 |
| 启动 20 秒后跑一轮，之后每 30 分钟 | 开机即签；跨过 0 点也能自动补 |
| 当天已有结论的账号直接跳过 | 9 个账号 × 每 30 分钟不该产生无意义的请求 |
| 失败账号 6 小时内不重试 | 凭据失效的账号不该被反复锤；手动签到不受限 |
| 不伪造 `X-Device-Token` | 那是客户端 Turing SDK 的反爬头；实测缺它也能查、能签 |

记录写在 `~/.workbuddy-hub/checkin.json`（每账号的日期、成败、积分、连签天数）。
默认自动开启，可随时关掉或点「立即签到」。

```bash
hub checkin --status          # 只看，不领
hub checkin                   # 逐账号领取
hub checkin --uid 128a8f9c    # 只处理一个账号
```

### 托盘、自启与单实例

窗口是**无边框**的：顶部这一行就是标题栏，拖空白处可以移动窗口，双击最大化 / 还原；
最右侧依次是主题切换和「最小化 / 最大化 / 关闭」。

关窗不再退出进程，只隐藏到托盘 —— 切号和签到调度都需要它活着。
托盘左键点击显示窗口，右键菜单「显示主窗口 / 退出」；Alt+F4 同样是隐藏到托盘。

开机自启没有引入插件，直接写注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
的 `WorkBuddyHub` 项：只影响当前用户，不需要管理员权限，卸载时删掉该项即可。

重复启动不会开出第二个实例：第二个进程通过 `127.0.0.1:47821` 通知已有实例把窗口
拉到前台，然后自己退出。端口被别的程序占用时照常启动，只是失去互斥能力。

---

## 界面与主题

右上角三档：**跟随系统 / 浅色 / 深色**，选择存在 `localStorage`。

| 深色 | 浅色 |
|---|---|
| ![深色](docs/ui-dark.png) | ![浅色](docs/ui-light.png) |

- 「跟随系统」不写死结果，而是监听 `prefers-color-scheme` —— 系统到点切夜间模式，界面跟着变
- 浅色不是把深色反相：底 `#f4f6fc`、卡片纯白、阴影变浅、状态色整体压深一档保证对比度
- 账号头像是从 uid 派生的渐变色，同一个账号每次都是同一个颜色
- 启动前先在内联脚本里定主题，浅色系统下不会先闪一下黑屏

任务详情：

![任务详情](docs/ui-detail.png)

---

## 命令行

`hub` 在 `target/debug/hub`（`cargo build` 后）。

```bash
hub probe                 # 探测客户端位置、凭据文件、数据目录
hub accounts              # 账号库总览：任务数、凭据到期、哪些是空号
hub tasks                 # 所有账号的任务（--uid <前缀> 过滤）
hub verify                # 用 at-rest 密钥逐个解密账号凭据
hub switch --to 697af213  # 切号（默认演练，--force 才写文件）
hub unify [--force|--status|--rollback]
hub watch [--once|--auto]
hub transcript <id>       # 预览会话正文（--tail N 控制条数）
hub checkin [--status]    # 每日签到
hub authhash              # 输出凭据哈希，用于与 TS 实现对拍
```

测试：

```bash
cargo test     # 30 个单测：AAD 布局、加解密往返、原子写、备份清理、正文解析、签到记录、手机号脱敏
```

---

## 目录

```
workbuddy-hub/
├─ Cargo.toml            # workspace（core + cli）
├─ core/                 # 纯逻辑，可单测，不依赖 Tauri
│  └─ src/
│     ├─ client_compat.rs   客户端探测 / at-rest 密钥
│     ├─ client_control.rs  退出 / 等待 / 启动客户端
│     ├─ auth_crypto.rs     AES-256-GCM + 自定义 AAD
│     ├─ account_store.rs   账号库读取（兼容秒/毫秒）
│     ├─ task_index.rs      只读聚合任务
│     ├─ task_unify.rs      任务归集 + 原始归属表 + 回滚
│     ├─ rate_limit.rs      从 jsonl 解析限流信号
│     ├─ transcript.rs      会话正文预览（消息 / 工具调用 / 统计）
│     ├─ checkin.rs         每日签到（curl + 本地记录）
│     ├─ daemon.rs          冷却表 / 选号 / 换号编排
│     └─ switch_engine.rs   切号：备份 / 原子写 / 回滚
├─ cli/                  # 命令行验证工具
│  └─ src/main.rs
├─ src/                  # React 前端
│  ├─ App.tsx               任务总览 + 账号栏 + 限流提示 + 签到卡片
│  ├─ TaskDetail.tsx        对话预览抽屉
│  ├─ theme.ts              主题（跟随系统 / 浅色 / 深色）
│  ├─ api.ts                invoke 封装（浏览器里回退到示例数据）
│  └─ styles.css            双主题设计系统
├─ src-tauri/            # Tauri 2 壳
│  ├─ src/lib.rs            21 个 IPC 命令 + 托盘 + 签到调度
│  ├─ icons/tray.png        托盘图标（32×32）
│  └─ tauri.conf.json
├─ docs/                 # 技术说明与设计方案
└─ tools/                # M1 对拍脚本
```

## 运行时路径

| 用途 | 路径 |
|---|---|
| 会话索引 | `~/.workbuddy/workbuddy.db` → `sessions` 表 |
| 会话正文 | `~/.workbuddy/projects/<编码 cwd>/<session-id>.jsonl` |
| 客户端凭据 | `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info` |
| 账号库（复用 DSH） | `~/.dsh/workbuddy-accounts/<uid>.json` |
| hub 状态 | `~/.workbuddy-hub/`：`ownership.json`、`runtime.json`、`checkin.json`、`profiles.json`、`backups/` |

凭据格式与加解密细节见[技术说明第十节](docs/workbuddy-hub-技术说明.md)。

---

## 常见问题

**窗口纯黑，只有标题栏。**
先确认不是启动环境的问题：从被注入了沙箱组件的终端启动，WebView2 子进程会被注入
第三方 DLL（这台机器上是 `tsbx.dll`），渲染进程随即崩溃。判断方法：

```powershell
Get-ChildItem "$env:LOCALAPPDATA\cn.workbuddy.hub" -Recurse -Filter *.dmp |
  Sort-Object LastWriteTime -Descending | Select-Object -First 1
```

每次启动都新增 `.dmp` 就是这个原因。注入沿**进程树**传播，清环境变量没用 ——
必须从干净的父进程（桌面、开始菜单）启动。

**`cargo build` 报 TLS / 502。**
本机代理会拒绝 crates.io，需要配镜像（`~/.cargo/config.toml` 指向 rsproxy）。

**链接时提示找不到 `link.exe` 或参数错误。**
Git 自带的 `usr/bin/link.exe` 会抢在 MSVC 链接器前面，把 MSVC 的
`bin/Hostx64/x64` 放到 PATH 最前面。

**启动或切号时闪过一串黑框。**
已修：所有外部命令（`tasklist` / `taskkill` / `powershell` / `curl`）都通过
`hub_core::proc::hidden()` 构造，带 `CREATE_NO_WINDOW`。启动时账号资料和签到
各要发 9 次 `curl`，同样不会闪窗。

**签到全部失败。**
先 `hub checkin --status` 看服务端返回。常见原因是账号凭据过期（客户端重新登录一次即可），
或系统里没有 `curl`（Windows 10 1803 之前）。

---

## 踩过的坑（改代码前先看）

1. **时间戳单位不统一。** 账号库里 9 个存档的 `expiresAt` 是毫秒，1 个是秒。
   直接按秒算会把有效期算成 56845 年，于是永远「未过期」。
   所有时间字段都必须过 `normalize_ts()`（大于 `1e11` 视为毫秒）。
2. **客户端不在默认路径。** 探测要覆盖 `WORKBUDDY_ELECTRON_BIN` → 运行中进程 →
   Program Files → 常见盘符根目录。
3. **`link.exe` 冲突。** 见上。
4. **客户端有单实例锁**（`app/lockfile`），切号必须退出 → 改凭据 → 重启，不能热切换。
5. **账号库里的副本文档。** `.bak` / `.imported-*` / `baseline-*` 都不是规范条目，
   只认 `<uuid>.json`；同一 uid 出现多次时保留规范文件。
6. **凭据文件没有完整性校验。** 旁边的 `observation.json` 只是活跃会话标记
   （`facts: []`），改写 auth 文件不会触发校验失败 —— 但没有校验也意味着
   写错了只能靠客户端的登录失败来发现，所以切号必须做备份与回滚。
7. **`sessions.user_id` 不是「归属真相」。** 归集会抹平这个字段，
   hub 界面按 `ownership.json` 显示真实归属；回滚也以它为准。

## 安全边界

- 账号库是**明文**（按既定决定不做加密）。它等同于这些账号的登录凭据，
  不要提交到 git、不要放共享目录。
- 只管理**自己名下**已登录过的账号，不做账号分发、不做代登录。
- 所有账号操作仅限本机，不上传任何凭据。
- 外发请求只有签到与账号资料两处：token 只发给 `https://<账号自己的 domain>`，
  不经过任何第三方，也不写入日志或临时文件。

## 开发进度

| 阶段 | 内容 | 状态 |
|---|---|---|
| M0 | 只读聚合所有账号的任务 | **已验证**：9 个账号任务全部列出 |
| M1 | 用 at-rest 密钥解密客户端凭据 | **已验证**：双向对拍一致，9/9 账号解密成功 |
| M2 | 切号（备份 → 写凭据 → 回滚） | **已实测**：账号切换后客户端正常 |
| M3 | 桌面壳 + 任务总览界面 | **已运行**：Tauri 2 窗口，常驻约 40 MB |
| M3.5 | 任务归集 | **已实测**：13 个任务合并到目标账号，归属表 19 条；换号后任务列表与上下文完整 |
| M4 | 限流检测（L1 半自动） | 检测已验证；`--auto` 换号未真实触发 |
| M5 | 任务详情 | **已实现**：抽屉预览，工具调用可展开 |
| M6 | 托盘常驻 + 开机自启 + 单实例 | **已实现** |
| M7 | 界面重做 + 双主题 | **已实现**：三档主题，跟随系统 |
| M8 | 每日自动签到 | 9 个账号状态读取成功；领取分支待次日验证 |

还没做的：

- `watch --auto` 的真实换号（逻辑就绪，等一个手头没有任务的时机）
- 改完 `user_id` 后能否不重启客户端就刷新列表（需要一次实测）
- L2 MITM 代理（真无感切换）：需要装根证书，未采纳

## 许可

MIT，见 [LICENSE](LICENSE)。
