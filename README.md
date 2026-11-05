# kiro-rs

一个用 Rust 编写的 Anthropic Claude API 兼容代理服务，将 Anthropic API 请求转换为 Kiro API 请求。

---

<table>
<tr>
<td>
<b>特别感谢</b>：<a href="https://co.yes.vg/register?ref=hank9999">YesCode</a> 为本项目提供了 AI API 额度赞助, YesCode 作为一家低调务实的 AI API 中转服务商 <br>
长期以来提供稳定高可用的服务, 如您有意体验, 请点击链接注册体验 → <a href="https://co.yes.vg/register?ref=hank9999">立即访问</a>
</td>
</tr>
</table>

---

#### [LINUX DO 讨论帖](https://linux.do/t/topic/1571986)

## 免责声明

本项目仅供研究使用, Use at your own risk, 使用本项目所导致的任何后果由使用人承担, 与本项目无关。
本项目与 AWS/KIRO/Anthropic/Claude 等官方无关, 本项目不代表官方立场。

## 注意！

因 TLS 默认从 native-tls 切换至 rustls，你可能需要专门安装证书后才能配置 HTTP 代理。可通过 `config.json` 的 `tlsBackend` 切回 `native-tls`。
如果遇到请求报错, 尤其是无法刷新 token, 或者是直接返回 error request, 请尝试切换 tls 后端为 `native-tls`, 一般即可解决。

**Write Failed/会话卡死**: 已增加分块写入提示与工具参数校验，详见下方[大文件写入与截断处理](#大文件写入与截断处理)。上游仍可能截断长输出；`max_tokens` 未映射到 Kiro 的原生生成上限，调低它不能保证避免截断。历史讨论见 Issue [#22](https://github.com/hank9999/kiro.rs/issues/22)（包含 Windows 路径问题）和 [#49](https://github.com/hank9999/kiro.rs/issues/49)（长输出截断）。

## 功能特性

- **Anthropic API 兼容**: 完整支持 Anthropic Claude API 格式
- **流式响应**: 支持 SSE (Server-Sent Events) 流式输出
- **Token 自动刷新**: 自动管理和刷新 OAuth Token
- **多凭据支持**: 支持配置多个凭据，按优先级自动故障转移
- **负载均衡**: 默认 `balanced`（按在途请求数、历史成功次数、优先级依次选择），支持 `priority`（优先复用当前账号，满载/冷却时分流）模式
- **会话软亲和**: 默认优先复用同一会话上次成功的账号，满载/冷却或不符合模型资格时立即分流，保留公平排队
- **并发保护**: 全局/单账号在途上限、有界等待及账号级 429 冷却，取消和流结束自动释放名额
- **请求保护**: 限制完整请求的并发与总耗时，约束上游响应大小、首内容等待和空闲时间；覆盖对话及 WebSearch/MCP
- **智能重试**: 每轮上游 HTTP 尝试最多 `min(凭据数 × 3, 9)` 次；空回复另有最多 2 次重试
- **凭据回写**: 自动原子回写刷新后的 Token 和管理变更，支持单凭据与多凭据格式
- **Thinking 模式**: 支持 Claude 的 extended thinking 功能
- **工具调用**: 完整支持 function calling / tool use
- **WebSearch**: 内置 WebSearch 工具转换逻辑
- **多模型支持**: 支持 Sonnet、Opus、Haiku 系列模型
- **Admin 管理**: 可选的 Web 管理界面和 API，支持凭据管理、余额查询等
- **调用日志**: SQLite 持久化逐次调用，管理页可查询用量、耗时、错误原因与重试记录
- **多级 Region 配置**: 支持全局和凭据级别的 Auth Region / API Region 配置
- **凭据级代理**: 支持为每个凭据单独配置 HTTP/SOCKS5 代理，优先级：凭据代理 > 全局代理 > 无代理

---

- [开始](#开始)
  - [1. 编译](#1-编译)
  - [2. 最小配置](#2-最小配置)
  - [3. 启动](#3-启动)
  - [4. 验证](#4-验证)
  - [Docker](#docker)
- [配置详解](#配置详解)
  - [config.json](#configjson)
  - [credentials.json](#credentialsjson)
  - [Region 配置](#region-配置)
  - [代理配置](#代理配置)
  - [认证方式](#认证方式)
  - [环境变量](#环境变量)
- [API 端点](#api-端点)
  - [标准端点 (/v1)](#标准端点-v1)
  - [Claude Code 兼容端点 (/cc/v1)](#claude-code-兼容端点-ccv1)
  - [Thinking 模式](#thinking-模式)
  - [工具调用](#工具调用)
- [模型映射](#模型映射)
- [Admin（可选）](#admin可选)
- [注意事项](#注意事项)
- [项目结构](#项目结构)
- [技术栈](#技术栈)
- [License](#license)
- [致谢](#致谢)

## 开始

### 1. 编译

> PS: 如果不想编辑可以直接前往 Release 下载二进制文件

> **前置步骤**：编译前需要先构建前端 Admin UI（用于嵌入到二进制中）：
> ```bash
> cd admin-ui && pnpm install && pnpm build
> ```

```bash
cargo build --release
```

### 2. 最小配置

创建 `config.json`：

```json
{
   "host": "127.0.0.1",
   "port": 8990,
   "apiKey": "sk-kiro-rs-qazWSXedcRFV123456",
   "region": "us-east-1"
}
```
> PS: 如果你需要 Web 管理面板, 请注意配置 `adminApiKey`

创建 `credentials.json`（从 Kiro IDE 等中获取凭证信息）：
> PS: 可以前往 Web 管理面板配置跳过本步骤
> 如果你对凭据地域有疑惑, 请查看 [Region 配置](#region-配置)

Social 认证：
```json
{
   "refreshToken": "你的刷新token",
   "expiresAt": "2025-12-31T02:32:45.144Z",
   "authMethod": "social"
}
```

IdC 认证：
```json
{
   "refreshToken": "你的刷新token",
   "expiresAt": "2025-12-31T02:32:45.144Z",
   "authMethod": "idc",
   "clientId": "你的clientId",
   "clientSecret": "你的clientSecret",
     "profileArn": "你的profileArn（可选，部分企业IdC必填）"
}
```

### 3. 启动

```bash
./target/release/kiro-rs
```

或指定配置文件路径：

```bash
./target/release/kiro-rs -c /path/to/config.json --credentials /path/to/credentials.json
```

### 4. 验证

```bash
curl http://127.0.0.1:8990/v1/messages \
  -H "Content-Type: application/json" \
  -H "x-api-key: sk-kiro-rs-qazWSXedcRFV123456" \
  -d '{
    "model": "claude-sonnet-4-20250514",
    "max_tokens": 1024,
    "stream": true,
    "messages": [
      {"role": "user", "content": "Hello, Claude!"}
    ]
  }'
```

存活探针 `GET /healthz` 固定返回 `200`；就绪探针 `GET /readyz` 在至少有一个启用的凭据时返回 `200`，否则返回 `503`。两者均无需 API Key，且不主动调用上游验证凭据。停机收到 SIGINT/SIGTERM 后最多等待 30 秒完成在途请求。

### Docker

通过 Docker 从当前源码构建并启动（包含本地修改的管理页面和日志功能）：

```bash
docker compose up -d --build
```

将 `config.json` 和 `credentials.json` 放在宿主机 `./config/` 目录，Compose 将整个目录挂载至 `/app/config/`。调用日志数据库和统计缓存也保存在这个目录，重建容器不会丢失。访问 `http://localhost:8990/admin` 需要在配置中设置 `adminApiKey`，配置的监听地址应为 `0.0.0.0`、端口为 `8990`。

Compose 默认镜像名为 `abu116/kiro-rs:latest`，与 `docker-push.sh` 一致；可通过 `IMAGE_NAME`、`IMAGE_TAG` 覆盖。若使用已推送的镜像，在发布新版本后执行 `docker compose pull && docker compose up -d --no-build`。尚未推送的本地修改使用上面的 `--build` 命令。

终端运行日志通过 `docker compose logs -f --tail 200 kiro-rs` 查看；Compose 为其配置了每份 10 MB、最多 3 份的轮转限制。数据库中的逐次调用日志通过管理页面查询，保留时间由 `requestLogRetentionDays` 控制。

#### 构建并推送到 Docker Hub

使用项目根目录的脚本，一次构建并推送 `linux/amd64`、`linux/arm64` 多架构镜像到 `abu116/kiro-rs`，拉取时 Docker 会自动选择对应架构。

```bash
# 首次使用先登录具有仓库推送权限的 Docker Hub 账号
docker login -u abu116

# 默认推送 abu116/kiro-rs:latest
./docker-push.sh

# 同一次构建推送版本标签和 latest
./docker-push.sh 2026.10.1 latest
```

需要启动 Docker 并安装 Buildx（Docker Desktop 已包含）。脚本会自动创建并复用 `kiro-rs-multiarch` builder，也可通过 `BUILDER_NAME` 指定已配置的多架构 builder。Docker Desktop 默认支持跨架构模拟；Linux 主机若尚未配置 QEMU/binfmt，可先执行 `docker run --privileged --rm tonistiigi/binfmt --install amd64,arm64`。跨架构模拟编译 Rust 可能耗时较长。

## 配置详解

### config.json

| 字段 | 类型 | 默认值 | 描述 |
|------|------|--------|------|
| `host` | string | `127.0.0.1` | 服务监听地址 |
| `port` | number | `8080` | 服务监听端口 |
| `apiKey` | string | - | 自定义 API Key（用于客户端认证，必配） |
| `region` | string | `us-east-1` | AWS 区域 |
| `authRegion` | string | - | Auth Region（用于 Token 刷新），未配置时回退到 region |
| `apiRegion` | string | - | API Region（用于 API 请求），未配置时回退到 region |
| `kiroVersion` | string | `0.9.2` | Kiro 版本号 |
| `machineId` | string | - | 自定义机器码（64位十六进制），不定义则自动生成 |
| `systemVersion` | string | 当前运行平台 | 系统标识，默认 `darwin` / `win32` / `linux` 等；可显式配置完整版本 |
| `nodeVersion` | string | `22.21.1` | Node.js 版本标识 |
| `tlsBackend` | string | `rustls` | TLS 后端：`rustls` 或 `native-tls` |
| `countTokensApiUrl` | string | - | 外部 count_tokens API 地址 |
| `countTokensApiKey` | string | - | 外部 count_tokens API 密钥 |
| `countTokensAuthType` | string | `x-api-key` | 外部 API 认证类型：`x-api-key` 或 `bearer` |
| `countTokensTimeoutSeconds` | integer | `10` | 远程 token 计数总超时（秒，最小 1），失败时回退本地估算 |
| `proxyUrl` | string | - | HTTP/SOCKS5 代理地址 |
| `proxyUsername` | string | - | 代理用户名 |
| `proxyPassword` | string | - | 代理密码 |
| `adminApiKey` | string | - | Admin API 密钥，配置后启用凭据管理 API 和 Web 管理界面 |
| `requestLogPath` | string | 凭据文件同目录下的 `kiro_requests.sqlite3` | 调用日志 SQLite 数据库路径；相对路径基于进程工作目录 |
| `requestLogRetentionDays` | integer | `30` | 日志保留天数，最小 1；同时定期清理至最近 100000 条 |
| `loadBalancingMode` | string | `balanced` | `balanced` 按在途请求数、累计成功次数、优先级依次选择；`priority` 优先复用当前账号 |
| `requestLimits.maxUpstreamAttempts` | integer | `9` | 单个客户端请求所有 Kiro HTTP 尝试共用上限，含 Token 刷新、对话、搜索、压缩和响应恢复；范围 1～100 |
| `requestLimits.maxConcurrentRequests` | integer | `64` | 单进程 HTTP 请求并发上限；超限返回 `429` |
| `requestLimits.totalTimeoutSeconds` | integer | `900` | 单次请求总截止时间，覆盖处理和响应体读取，范围 1～86400 |
| `requestLimits.firstContentTimeoutSeconds` | integer | `180` | 对话等待上游首个有效内容的上限；MCP 等待首个非空数据块的上限，范围 1～86400 |
| `requestLimits.idleTimeoutSeconds` | integer | `120` | 上游响应相邻数据块之间的最长空闲时间，范围 1～86400 |
| `requestLimits.maxResponseBytes` | integer | `33554432` | 上游响应的字节上限，覆盖对话、MCP、Token 刷新与余额查询；账户接口另设 1 MiB 上限，错误正文最多读取此值与 64 KiB 中较小者，范围 1024～1073741824 |
| `requestLimits.maxResponseEvents` | integer | `100000` | 单次响应生成的事件上限，范围 1～1000000 |
| `sessionAffinity.enabled` | boolean | `true` | 两种调度模式均优先复用会话上次成功的账号；设为 `false` 恢复原调度规则 |
| `sessionAffinity.ttlSeconds` | integer | `1800` | 绑定自上次成功响应起的有效秒数，范围 1～86400 |
| `sessionAffinity.maxEntries` | integer | `10000` | 进程内绑定数量上限，范围 1～1000000；超限淘汰最久未成功的绑定 |
| `concurrency.maxInFlight` | integer | `64` | 全局上游在途调用上限，必须大于 0 |
| `concurrency.maxInFlightPerCredential` | integer | `8` | 单账号在途调用上限，必须大于 0 |
| `concurrency.maxWaiting` | integer | `128` | 等待名额的调用上限；`0` 表示满载/冷却时立即拒绝 |
| `concurrency.waitTimeoutSeconds` | integer | `30` | 每次获取调用名额的最长等待秒数，范围 1～86400 |
| `concurrency.rateLimitCooldownSeconds` | integer | `30` | 账号收到 429 后的最短冷却秒数，范围 1～86400；与 `Retry-After` 取较长者 |
| `concurrency.requestsPerMinute` | integer | `60` | 每账号滚动 60 秒的调用预留上限；0 关闭，最大 1000000 |
| `concurrency.refreshBackoffSeconds` | integer | `30` | 认证服务瞬态失败的初始退避秒数，带指数增长和抖动；范围 1～86400 |
| `concurrency.adaptiveConcurrency` | boolean | `true` | 启用账号级自适应并发：429 后降低容量，冷却后单请求探测，再逐步恢复至配置上限 |
| `ccBufferedStream` | boolean | `true` | `/cc/v1/messages` 是否缓冲完整流；设为 `false` 可实时返回内容 |
| `extractThinking` | boolean | `true` | 非流式响应的 thinking 块提取。启用后 `<thinking>` 标签会被解析为独立的 `thinking` 内容块 |
| `defaultEndpoint` | string | `ide` | 默认 Kiro 端点。凭据未显式指定 `endpoint` 时使用。当前支持：`ide` |

通过管理 API 切换负载均衡模式时，会先原子保存配置再更新运行模式；保存失败则保留原模式。配置回写保留未知扩展字段和已有文件权限，新建配置文件在 Unix 上使用 `0600` 权限。

全局及凭据级 `region`、`authRegion`、`apiRegion` 必须是 1～63 字节的小写 ASCII 字母、数字或连字符，首尾须为字母或数字。区域名称作为单个 DNS 标签使用，不接受 URL、域名、路径或空字符串；启动时拒绝无效配置，管理 API 添加凭据时返回 `400 invalid_request`，不回显非法值。

所有出站 HTTP 客户端均不自动跟随重定向，避免转发刷新令牌、客户端密钥、API Key 和请求正文。请直接配置远程 Token 计数服务的最终地址。对话与 MCP 收到上游 3xx 时立即返回上游错误（客户端 HTTP `502`），不重试、不计入账号认证失败，并释放并发名额；Token 刷新和余额查询同样将 3xx 视为失败。

并发保护在单进程内生效，普通对话、流式对话、压缩摘要和 WebSearch 共用名额。名额从选择账号开始持有，覆盖 Token 获取与上游响应体读取，流结束、丢弃响应或取消调用时释放；HTTP 重试退避期间不占名额。两种模式都会跳过满载或冷却中的账号，同时保留模型资格检查。Token 刷新按账号互斥，同账号自动刷新在获取锁后复查 Token，不同账号可并行刷新。

等待者在名额释放、账号管理变更或冷却到期后重新参与调度。对当前可调度的等待者按入队顺序分配名额，新请求不能插队；如果前面的调用暂时没有符合模型、容量和冷却条件的账号，后面的调用可使用空闲账号，避免 Opus 等请求阻塞整个队列。等待被取消、超时或出错时会移出队列并唤醒后续调用。队列满或等待超时会返回 `503 overloaded_error` 和 `Retry-After: 1`，不消耗 Provider 的 HTTP 重试预算；已经开始的 SSE 通过错误事件报告失败。等待上限针对每次获取上游名额，既不是整个客户端请求的总超时，也不限制处于退避中的客户端请求数。并发限制与账号 RPM 保护分别生效；两者均不等同于 TPM 或上游计费额度。

未指定 `loadBalancingMode` 的配置现在默认使用 `balanced`；显式设置的 `priority` 保留。并发参数修改后重启生效；指定凭据文件路径时，冷却状态会持久保存并在重启后恢复。多实例共享账号池时仍需额外的跨实例协调。

会话软亲和默认开启，在公平队列允许分配名额后，优先选择该会话上次成功使用且当前可调度的账号。账号满载、冷却、禁用、删除或不支持请求模型时，立即按原有 `balanced` / `priority` 规则选其他账号；不会专门等待绑定账号。没有有效绑定的请求沿用原调度规则，`priority` 的“当前账号”仍是全局偏好。仅在 Messages 响应完整且通过现有响应校验后建立或刷新绑定；空响应、解码失败、断流和取消不会更新绑定，分流请求成功后才改绑。同一会话并发请求按成功完成顺序更新绑定。

绑定键使用规范化 `conversationId` UUID 的哈希。客户端通过 `metadata.user_id` 中的 `session_<UUID>` 或 JSON `session_id` 提供稳定标识；没有稳定标识时转换器每次生成新 UUID，无法跨请求复用。绑定不依赖模型，但每次选账号仍检查模型资格；WebSearch 的 MCP 调用不建立会话绑定。绑定仅保存在当前进程，重启清空，设置修改后重启生效；只保存哈希、账号 ID 和成功时间，数量受上限约束。亲和不保证 Kiro 上游缓存命中，也不改变 token 用量统计。

账号级自适应并发默认开启，每个账号初始使用配置的单账号上限（默认 8）。收到 429 时将其恢复目标减半（向下取整，最小 1），保留原有冷却规则；同一批已在途请求陆续返回的 429 只触发一次降档，但每次仍会按 `Retry-After` 延长冷却。冷却结束后仅允许一个在途调用探测。探测的 2xx 响应体完整读完后恢复至减半后的目标，此后每完成一轮当前上限数量的响应，增加一个名额，直到达到配置上限。例如上限为 8 时，恢复过程为单请求探测 → 4 → 5 → 6 → 7 → 8。新一轮调用再次收到 429 会重新减半并探测，其他账号不受影响。满载判断和公平队列均使用实际生效的上限。

降低上限不会取消已在途的调用；调度器等待其释放名额后再允许探测。容量恢复以传输完成为依据，与业务响应校验和成功统计分开。取消、丢弃响应、响应体读取失败、限流前请求的迟到成功和克隆上下文的重复回报不会推动恢复；完整收到但业务内容无效的 2xx 仍可能推动容量恢复。该策略只控制并发，不保证满足上游 RPM/TPM 限额，状态在重启后重置。已有配置显式指定的上限和开关继续生效；省略字段时使用新的默认值。设为 `concurrency.adaptiveConcurrency: false` 可关闭自适应机制。

完整配置示例：

```json
{
   "host": "127.0.0.1",
   "port": 8990,
   "apiKey": "sk-kiro-rs-qazWSXedcRFV123456",
   "region": "us-east-1",
   "tlsBackend": "rustls",
   "kiroVersion": "0.9.2",
   "machineId": "64位十六进制机器码",
   "systemVersion": "darwin#24.6.0",
   "nodeVersion": "22.21.1",
   "authRegion": "us-east-1",
   "apiRegion": "us-east-1",
   "countTokensApiUrl": "https://api.example.com/v1/messages/count_tokens",
   "countTokensApiKey": "sk-your-count-tokens-api-key",
   "countTokensAuthType": "x-api-key",
   "proxyUrl": "http://127.0.0.1:7890",
   "proxyUsername": "user",
   "proxyPassword": "pass",
   "adminApiKey": "sk-admin-your-secret-key",
   "loadBalancingMode": "balanced",
   "requestLimits": {
      "maxConcurrentRequests": 64,
      "maxUpstreamAttempts": 9,
      "totalTimeoutSeconds": 900,
      "firstContentTimeoutSeconds": 180,
      "idleTimeoutSeconds": 120,
      "maxResponseBytes": 33554432,
      "maxResponseEvents": 100000
   },
   "concurrency": {
      "maxInFlight": 64,
      "maxInFlightPerCredential": 8,
      "maxWaiting": 128,
      "waitTimeoutSeconds": 30,
      "rateLimitCooldownSeconds": 30,
      "adaptiveConcurrency": true,
      "requestsPerMinute": 60,
      "refreshBackoffSeconds": 30
   },
   "extractThinking": true
}
```

账号主动限速默认开启：每个账号在滚动 60 秒内最多预留 60 次调用，对话、WebSearch、压缩及其 HTTP/空响应/工具恢复重试共用。预留在获取 Token 前计入；取消、刷新失败或未实际发送请求也不退还，以保守限制尝试数。额度用完后沿用有界公平队列，窗口到期自动唤醒；不占用在途名额。此限制允许窗口内突发，并非每秒匀速节流，也不是 Kiro 官方额度或 TPM 限制。认证刷新单独由互斥和退避控制。设置 `requestsPerMinute: 0` 可关闭 RPM 保护。

认证服务发生瞬态故障后，至少等待 `refreshBackoffSeconds`，连续失败时指数增长并加抖动，与有效 `Retry-After` 取更长者。退避在同账号刷新锁内登记，等待锁的请求、管理接口强制刷新和余额查询均不能跳过；不同账号互不阻塞。并发请求因同一个旧 access token 被拒绝时，锁内发现已更新就复用新 Token，不重复刷新。

普通 API/MCP 的非 Token 失效 403 会立即停用账号并终止当前调用，保留上游 403；401 连续失败达到 3 次停用。自动停用原因与 `disabled` 一并写回凭据文件；写入失败会记录错误。所有账号或某个模型都无可用凭据时，也不会重新启用失败账号。排查权限、订阅或配置后，通过管理接口手动启用。凭据加载和新增验证前会规范化并固定 `machineId`，刷新 Token 不改变设备 ID；未配置的 `systemVersion` 使用实际运行平台，不再随机选择系统。

客户端请求的 `maxUpstreamAttempts` 贯穿处理过程及响应流读取，额外恢复不会重置预算；预算耗尽时 JSON 返回 `503 overloaded_error`，已开始的 SSE 发送错误事件。总截止时间仍由 `totalTimeoutSeconds` 约束。管理后台独立请求不共享该客户端预算。`cooldownRemainingMs` / `coolingCredentials` 同时反映 429、刷新退避和 RPM 等待；容量估计计入当前 RPM 剩余额度，`concurrency` 新增 `requestsPerMinute` / `refreshBackoffSeconds`。

指定凭据文件路径时，RPM 窗口、429 冷却、刷新退避及其连续失败次数、429 后的并发降档会持久保存到凭据文件旁的 `<凭据文件名>.protection.sqlite3`（例如 `credentials.json.protection.sqlite3`），无需额外配置。调用预留在获取 Token/发送请求前同步提交，冷却、退避及并发档位变化也及时写入，不依赖正常退出。此文件不保存 Token、API Key 或邮箱；按凭据 ID 和稳定身份摘要匹配，OAuth Token 轮换不会清空保护。重启时扣除停机时间，过期窗口自动失效；系统时钟回拨时保留保存时的剩余等待。并发成功窗口的累计进度在重启后从零开始，已降低的并发档位及探测状态保留。

保护文件所在目录须可写；初始化/读取损坏失败会阻止启动，运行中写入失败时新的调用返回 `503 overloaded_error`，不继续发送上游请求。每次预留涉及同步磁盘提交，应使用可靠的本地持久盘。备份、迁移时与凭据文件一起保留；Docker Compose 默认已挂载整个配置目录。未指定凭据文件路径时保护仅在内存生效。此机制只解决单实例重启恢复，多实例共享账号仍需额外协调；不要让多个进程同时使用同一凭据和保护文件。

### credentials.json

支持单对象格式（向后兼容）或数组格式（多凭据）。两种格式都会保存 Token 刷新和管理变更；单对象文件添加第二个凭据或删除最后一个凭据时，会自动转为数组格式。回写使用同目录临时文件和原子替换，保留已有文件权限。

Social 和 IdC 刷新会拒绝空的访问令牌、空的轮换刷新令牌，以及非正数或超出时间范围的 `expiresIn`，异常结果不会覆盖原凭据。HTTP 400 响应中 OAuth `error` 字段为 `invalid_grant` 时立即停用；其他明确的 4xx 认证错误按失败阈值处理。429、408、5xx、网络及无效响应进入账号级刷新退避，不计入永久停用阈值。Token 刷新和余额查询保留 60 秒总超时，响应体首包与空闲时间同时受 `requestLimits` 中相应设置与 60 秒的较小值约束。

#### 字段说明

| 字段             | 类型     | 描述                                          |
|----------------|--------|---------------------------------------------|
| `id`           | number | 凭据唯一 ID（可选，仅用于 Admin API 管理；手写文件可不填）        |
| `accessToken`  | string | OAuth 访问令牌（可选，可自动刷新）                        |
| `refreshToken` | string | OAuth 刷新令牌                                  |
| `profileArn`   | string | AWS Profile ARN（可选，登录时返回）                   |
| `expiresAt`    | string | Token 过期时间 (RFC3339)                        |
| `authMethod`   | string | 认证方式：`social` 或 `idc`                       |
| `clientId`     | string | IdC 登录的客户端 ID（IdC 认证必填）                     |
| `clientSecret` | string | IdC 登录的客户端密钥（IdC 认证必填）                      |
| `priority`     | number | 凭据优先级，数字越小越优先，默认为 0                         |
| `region`       | string | 凭据级 Auth Region, 兼容字段                       |
| `authRegion`   | string | 凭据级 Auth Region，用于 Token 刷新, 未配置时回退到 region |
| `apiRegion`    | string | 凭据级 API Region，用于 API 请求                    |
| `machineId`    | string | 凭据级机器码（64位十六进制）                             |
| `email`        | string | 用户邮箱（可选，从 API 获取）                           |
| `proxyUrl`     | string | 凭据级代理 URL（可选，特殊值 `direct` 表示不使用代理）       |
| `proxyUsername`| string | 凭据级代理用户名（可选）                                |
| `proxyPassword`| string | 凭据级代理密码（可选）                                 |
| `endpoint`     | string | 凭据级端点名称（可选，未配置时使用 `config.defaultEndpoint`）|

说明：
- IdC / Builder-ID / IAM 在本项目里属于同一种登录方式，配置时统一使用 `authMethod: "idc"`
- 为兼容旧配置，`builder-id` / `iam` 仍可被识别，但会按 `idc` 处理

#### 单凭据格式（旧格式，向后兼容）

```json
{
   "accessToken": "请求token，一般有效期一小时，可选",
   "refreshToken": "刷新token，一般有效期7-30天不等",
   "profileArn": "arn:aws:codewhisperer:us-east-1:111112222233:profile/QWER1QAZSDFGH",
   "expiresAt": "2025-12-31T02:32:45.144Z",
   "authMethod": "social",
   "clientId": "IdC 登录需要",
   "clientSecret": "IdC 登录需要"
}
```

#### 多凭据格式（支持故障转移和自动回写）

```json
[
   {
      "refreshToken": "第一个凭据的刷新token",
      "expiresAt": "2025-12-31T02:32:45.144Z",
      "authMethod": "social",
      "priority": 0
   },
   {
      "refreshToken": "第二个凭据的刷新token",
      "expiresAt": "2025-12-31T02:32:45.144Z",
      "authMethod": "idc",
      "clientId": "xxxxxxxxx",
      "clientSecret": "xxxxxxxxx",
      "region": "us-east-2",
      "priority": 1,
      "proxyUrl": "socks5://proxy.example.com:1080",
      "proxyUsername": "user",
      "proxyPassword": "pass"
   },
   {
      "refreshToken": "第三个凭据（显式不走代理）",
      "expiresAt": "2025-12-31T02:32:45.144Z",
      "authMethod": "social",
      "priority": 2,
      "proxyUrl": "direct"
   }
]
```

多凭据特性：
- 按 `priority` 字段排序，数字越小优先级越高（默认为 0）
- 每轮上游 HTTP 尝试预算为 `min(凭据数 × 3, 9)`；空回复最多额外重试 2 次，每次使用独立 HTTP 尝试预算
- 自动故障转移到下一个可用凭据
- Token 刷新和管理变更自动回写到源文件，支持单对象与数组格式

### Region 配置

支持多级 Region 配置，分别控制 Token 刷新和 API 请求使用的区域。

**Auth Region**（Token 刷新）优先级：
`凭据.authRegion` > `凭据.region` > `config.authRegion` > `config.region`

**API Region**（API 请求）优先级：
`凭据.apiRegion` > `config.apiRegion` > `config.region`

### 代理配置

支持全局代理和凭据级代理，凭据级代理会覆盖该凭据产生的所有出站连接（API 请求、Token 刷新、额度查询）。

**代理优先级**：`凭据.proxyUrl` > `config.proxyUrl` > 无代理

| 凭据 `proxyUrl` 值 | 行为 |
|---|---|
| 具体 URL（如 `http://proxy:8080`、`socks5://proxy:1080`） | 使用凭据指定的代理 |
| `direct` | 显式不使用代理（即使全局配置了代理） |
| 未配置（留空） | 回退到全局代理配置 |

出站连接仅使用上述显式配置，不自动读取 `HTTP_PROXY`、`HTTPS_PROXY`、`ALL_PROXY`、`NO_PROXY` 或操作系统代理。`direct` 会跳过全局、环境和系统代理；需要代理的部署请设置 `proxyUrl`。管理接口中的 `proxyUrl` 仅展示协议、主机与端口，移除 URL 用户名、密码、路径、查询和片段；实际连接及持久化配置保留原值。日志脱敏也覆盖只有用户名的代理 URL。

凭据级代理示例：

```json
[
   {
      "refreshToken": "凭据A：使用自己的代理",
      "authMethod": "social",
      "proxyUrl": "socks5://proxy-a.example.com:1080",
      "proxyUsername": "user_a",
      "proxyPassword": "pass_a"
   },
   {
      "refreshToken": "凭据B：显式不走代理（直连）",
      "authMethod": "social",
      "proxyUrl": "direct"
   },
   {
      "refreshToken": "凭据C：使用全局代理（或直连，取决于 config.json）",
      "authMethod": "social"
   }
]
```

### 认证方式

客户端请求本服务时，支持两种认证方式：

1. **x-api-key Header**
   ```
   x-api-key: sk-your-api-key
   ```

2. **Authorization Bearer**
   ```
   Authorization: Bearer sk-your-api-key
   ```

`apiKey` 必须是非空的可打印 ASCII 字符串，不能含空白或逗号；无效配置会阻止服务启动。`adminApiKey` 使用相同规则，无效时不启用管理接口。Bearer scheme 不区分大小写。重复的 `x-api-key` 或 `Authorization`、空值、格式错误和两个认证头携带不同密钥时均返回 `401 authentication_error`；同时提供两个头时必须使用相同密钥。Cookie 和 URL 查询参数不参与认证。认证失败响应包含 `WWW-Authenticate: Bearer`。

管理接口的 JSON、路径和查询参数解析失败统一返回 JSON `error` 对象，保留 `400`／`413`／`415`／`422` 状态码，不回显无效参数值；认证先于参数解析执行。客户端 JSON 与 SSE 错误中的常见令牌、密钥字段和代理认证信息统一脱敏，错误消息最多保留 4096 个字符。

认证接口的成功及错误响应均设置 `Cache-Control: no-store` 和 `X-Content-Type-Options: nosniff`，包含 SSE；不影响流式传输。管理页面设置 CSP，脚本及 API 连接仅允许同源，禁止内联脚本、动态代码执行、外部页面嵌入及表单原生提交，并设置 `X-Frame-Options: DENY`、`Referrer-Policy: no-referrer`。组件定位需要的内联样式保留，静态资源沿用原有缓存策略。

### 环境变量

可通过环境变量配置日志级别：

```bash
RUST_LOG=debug ./target/release/kiro-rs
```

## API 端点

### 标准端点 (/v1)

| 端点 | 方法 | 描述 |
|------|------|------|
| `/v1/models` | GET | 获取可用模型列表 |
| `/v1/messages` | POST | 创建消息（对话） |
| `/v1/messages/count_tokens` | POST | 估算 Token 数量 |

### Claude Code 兼容端点 (/cc/v1)

| 端点 | 方法 | 描述 |
|------|------|------|
| `/cc/v1/messages` | POST | 创建消息（缓冲模式，确保 `input_tokens` 准确） |
| `/cc/v1/messages/count_tokens` | POST | 估算 Token 数量（与 `/v1` 相同） |

> **`/cc/v1/messages` 与 `/v1/messages` 的区别**：
> - `/v1/messages`：实时流式返回，`message_start` 中的 `input_tokens` 是估算值
> - `/cc/v1/messages`：缓冲模式，等待上游流完成后，用从 `contextUsageEvent` 计算的准确 `input_tokens` 更正 `message_start`，然后一次性返回所有事件
> - 等待期间会每 25 秒发送 `ping` 事件保活

设置 `"ccBufferedStream": false` 后，`/cc/v1/messages` 会与 `/v1/messages` 一样实时返回内容：首个 `message_start` 使用估算值，结束时的 `message_delta.usage.input_tokens` 使用上游上下文使用量计算的值（上游未提供时保留估算值）。默认仍为缓冲模式。代理侧上下文压缩仍会等待摘要及后续调用完成。

远程 token 计数通过异步请求共享连接池，`countTokensTimeoutSeconds` 同时限制连接和响应体读取时间，响应体最多读取 64 KiB。超时、超大响应、HTTP 非 2xx（包括重定向）或无效计数会自动回退本地估算。

### 大文件写入与截断处理

携带工具的请求会附加分块提示，即使没有 `system`；常见的 Write/Edit、`write_file`、`apply_patch`、Bash/`exec_command` 及对应 MCP 名称会在工具描述中收到额外提醒。建议每次生成少于 50 行、12000 字符，先建骨架再逐步填充。整次响应还会收到 `min(max_tokens, 4096)` 的软预算提示，包含思考和所有工具参数，避免多个小调用合计仍超长。这些是保守的提示策略，不是 Kiro 的实际输出上限或强制文件大小限制。

普通对话的 JSON、实时 SSE、`/cc/v1` 缓冲 SSE 共用工具校验：暂存工具事件，直到收到结束标记、参数是合法 JSON 对象且包含工具 schema 顶层 `required` 字段后再交给客户端；没有必填字段的工具仍允许空参数。并行交错的工具事件保留原顺序，所有尚未完成的调用通过校验后一起释放。工具暂存的事件 payload 合计最多 4 MiB；工具片段之间的文本一同暂存，其他文本仍实时返回，工具生成期间每 25 秒发送 ping 保活。

未向响应处理层释放任何事件时，工具参数截断可以自动缩块恢复：丢弃失败片段，在原始请求的当前用户消息中追加恢复指令，要求只生成一个完整的小调用（首次最多 20 行、4000 字符，第二次 10 行、2000 字符），保留原历史、工具 schema 和工具结果。配置 `"toolOutputRecoveryAttempts": 1` 默认恢复一次，允许 0～2，0 为关闭，修改后重启生效。恢复会额外调用模型并消耗额度，失败尝试保留在调用日志中。恢复只适用于已声明的工具、合法响应帧中的参数截断；已释放事件（包括实时正文或完整工具）、上下文已满、网络断流、损坏帧、缺少必填字段及非截断语法错误不会自动重放。即使使用缓冲或 JSON 模式，也沿用同样保守的边界。

超过恢复次数或不符合恢复条件时，返回带缩小分块建议的错误：JSON 为 `400 invalid_request_error`；已经开始的 SSE 为 `error` 事件，不伪造正常完成，也不发出失败工具的残缺参数。新增识别 Kiro `metadataEvent.stopReason=max_tokens`，与长度异常一起触发截断处理；普通文本达到长度限制时保留 `max_tokens` stop reason。这些措施通过缩小生成任务提高成功率，不能提高上游的硬上限，也不能保证模型遵守提示或客户端停止重试。

### 上下文压缩

`/v1/messages` 和 `/cc/v1/messages` 支持代理侧压缩：

- Claude Code 的传统 `/compact` 总结请求仍返回普通文本摘要。
- 按需压缩：传入 `"compaction": {"type": "summarize"}`，返回单个 `compaction` 内容块，`stop_reason` 为 `compaction`。
- 阈值压缩：传入 `"context_management": {"edits": [{"type": "compact_20260112", "trigger": {"type": "input_tokens", "value": 150000}, "pause_after_compaction": true}]}`。省略 trigger 时默认 150000，最小值 50000；省略 pause_after_compaction 时会生成摘要后继续回答。
- 支持自定义 `instructions`、JSON / SSE 响应，以及后续请求中的摘要历史替换。压缩请求不能同时携带 `compaction` 和 `context_management`。

Kiro 没有原生 Anthropic 压缩接口，摘要由代理额外调用当前模型生成，会消耗额度。阈值依赖 token 估算；应在上下文满之前压缩。图片在摘要输入中用占位文本表示；按需摘要的 `kiro-rs:` 签名是代理内容校验值，不能用于 Anthropic 官方接口。无论是否启用压缩，都不会静默截断历史。

协议参考：[按需压缩](https://platform.claude.com/docs/en/build-with-claude/compaction-on-demand)、[阈值压缩](https://platform.claude.com/docs/en/build-with-claude/compaction-threshold)。

### Thinking 模式

默认 `ide` 端点沿用旧版提示词标签实现 thinking，恢复升级前的请求协议；这属于提示词引导，不保证精确的推理强度或预算。用户显式传入的设置优先于模型名中的 `-thinking` 别名。普通请求不会附加原生 thinking/effort 字段，历史推理也不会以新版字段发送给旧端点。

支持 Claude Code 的标准强度字段 `output_config.effort`，无需给模型名加 `-thinking`。强度会独立转换为 `<thinking_effort>` 提示词；仅传 effort、使用固定预算 thinking 或显式关闭 thinking 时也会保留强度，不会因为 effort 自动开启思考输出。当前支持的级别按模型校验，不支持的值返回 `400 invalid_request_error`。

当 `thinking.type` 为 `adaptive`（包括 `-thinking` 别名自动启用）且没有传非空 effort 时，默认值为：Haiku 5.5、Sonnet 5.5、Opus 5.5 使用 `medium`，Opus 4.7 使用 `xhigh`，其余使用 `high`。显式 effort 始终优先。完全不传 thinking/effort 且没有使用 `-thinking` 别名时，代理不添加控制参数，沿用 Kiro 自身行为。`enabled` 未传 `budget_tokens` 时默认 20000，默认 IDE 提示词预算最多 24576。

Claude Code 可直接连接 `/cc` 前缀（客户端会追加 `/v1/messages`），例如：

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:8990/cc"
export ANTHROPIC_AUTH_TOKEN="你的代理 apiKey"
claude --model claude-haiku-5-5 --effort high
```

会话内可用 `/effort` 调整强度，也可通过 `CLAUDE_CODE_EFFORT_LEVEL=high` 设置。客户端需升级到支持所选模型及 effort 的版本；Haiku 5.5 需要 Claude Code v2.1.293 或更高。参数格式与客户端设置参考 [Anthropic effort 文档](https://platform.claude.com/docs/en/build-with-claude/effort)及 [Claude Code 模型配置](https://code.claude.com/docs/en/model-config#adjust-effort-level)。

只有验证过当前 Kiro 账号、区域支持原生参数后，才设置 `"endpoints":{"ide":{"nativeThinking":true}}`。此模式通过 `additionalModelRequestFields` 传递 thinking/effort；`enabled` 会同时传递 `budget_tokens`，是否接受由上游模型 schema 决定。历史签名按 `reasoningContent.reasoningText.{text,signature}` 传递，加密块按 `reasoningContent.redactedContent` 传递；不拼接多个签名对应的文本。该模式尚未经过真实 Kiro 调用验证。

上下文窗口使用项目内维护的模型映射：已确认的 1M 模型会在 `GET /v1/models` 的 `max_input_tokens` 和 `GET /v1/models/{model_id}` 中声明 1,000,000。运行时 Kiro `contextUsageEvent` 只提供百分比，响应 `input_tokens` 是按窗口换算的估值；未收到事件时回退到本地 token 估算。上游拒绝 1M 请求时，代理无法扩展 Kiro 账号或模型本身的限制。

当前 `max_tokens` 仅用于本地响应/压缩处理，没有映射到 Kiro 的原生生成上限字段；输出仍由 Kiro 模型策略决定，达到上游长度限制时会返回 `max_tokens` stop reason。`tool_choice`、temperature、top_p、stop_sequences 和 JSON schema 输出也尚未映射到已确认的 Kiro 原生字段。

原生 Claude 请求示例：

```json
{
  "model": "claude-sonnet-4-20250514",
  "max_tokens": 16000,
  "thinking": {
    "type": "enabled",
    "budget_tokens": 10000
  },
  "messages": [...]
}
```

### 工具调用

完整支持 Anthropic 的 tool use 功能：

```json
{
  "model": "claude-sonnet-4-20250514",
  "max_tokens": 1024,
  "tools": [
    {
      "name": "get_weather",
      "description": "获取指定城市的天气",
      "input_schema": {
        "type": "object",
        "properties": {
          "city": {"type": "string"}
        },
        "required": ["city"]
      }
    }
  ],
  "messages": [...]
}
```

## 模型映射

| Anthropic 模型 | Kiro 模型 |
|----------------|-----------|
| `*fable*` | `claude-fable-5.1` |
| `*sonnet-5*` | `claude-sonnet-5` |
| `*sonnet-5-5*` / `*sonnet-5.5*` | `claude-sonnet-5.5` |
| `*sonnet*`（含 4.6/4-6） | `claude-sonnet-4.6` |
| `*sonnet*`（含 4.5/4-5） | `claude-sonnet-4.5` |
| `*opus-5-5*` / `*opus-5.5*` | `claude-opus-5.5` |
| `*opus-5*` | `claude-opus-5` |
| `*opus*`（含 4.8/4-8） | `claude-opus-4.8` |
| `*opus*`（含 4.7/4-7） | `claude-opus-4.7` |
| `*opus*`（含 4.6/4-6） | `claude-opus-4.6` |
| `*opus*`（含 4.5/4-5） | `claude-opus-4.5` |
| `*haiku*` | `claude-haiku-4.5` |
| `*haiku-5-5*` / `*haiku-5.5*` | `claude-haiku-5.5` |

Sonnet 5 的 thinking 行为与已知限制见 [docs/claude-sonnet-5.md](docs/claude-sonnet-5.md)。

## Admin（可选）

当 `config.json` 配置了非空 `adminApiKey` 时，会启用：

- **Admin API（认证同 API Key）**
  - `GET /api/admin/credentials` - 获取所有凭据状态
  - `POST /api/admin/credentials` - 添加新凭据
  - `DELETE /api/admin/credentials/:id` - 删除凭据
  - `POST /api/admin/credentials/:id/disabled` - 设置凭据禁用状态
  - `POST /api/admin/credentials/:id/priority` - 设置凭据优先级
  - `POST /api/admin/credentials/:id/reset` - 重置失败计数
  - `GET /api/admin/credentials/:id/balance` - 获取凭据余额

- **Admin UI**
  - `GET /admin` - 访问管理页面（需要在编译前构建 `admin-ui/dist`）

余额缓存的新鲜期为 5 分钟，同时校验凭据 ID 和认证信息的哈希；更换账户、缓存时间异常或旧缓存缺少身份信息时重新查询。管理页打开或刷新后直接展示上次成功查询的用量和更新时间，过期记录在服务重启后也会保留。页面可见时，后台按顺序更新当前页已启用账号的过期或缺失用量；失败时保留旧值并提示，自动重试间隔至少 5 分钟。“刷新本页用量”和余额弹窗的“刷新余额”会跳过缓存，对应 API 为 `GET /api/admin/credentials/:id/balance?force=true`。凭据列表 API 的 `balance` 字段包含缓存，余额的 `updatedAt` 为上次成功查询的 Unix 秒时间戳。

统计和余额缓存均使用同目录临时文件原子替换，新文件在 Unix 上使用 `0600` 权限。统计保存会串行处理并保留写入期间的新更新，保存失败后仍会重试。

管理首页每 5 秒刷新并发状态：全局在途/等待数量及上限、等待中的 Opus/其他调用数量、账号冷却/满载数量、排队拒绝/超时次数、429 次数和排队耗时（均值、近似 P50/P95、最大值）；账号列表展示各账号在途数量与剩余冷却时间。“已启用凭据”不等于此刻可调度，冷却与满载账号数可重叠。

空余容量通过 `concurrency.globalAvailableSlots`（全局剩余名额）、`availableSlots`（通用调用可用名额）和 `availableOpusSlots`（Opus 可用名额）展示。账号池可用名额按未禁用、冷却已结束账号的生效上限减去在途数计算，再受全局剩余名额约束；Opus 仅计支持该模型的账号。两类容量共享全局资源，不能相加。全局有空余而账号池容量为零时，应检查账号冷却、满载及模型资格。指标是采样时的容量估计，包含尚未结束的已删除账号对全局名额的占用；既有等待者仍按公平队列优先调度，空余数不保证新请求立即获准，也不预测 Token 刷新是否成功。

会话亲和指标同样通过 `GET /api/admin/credentials` 的 `concurrency` 返回：`sessionAffinityEnabled` 为开关；`affinityHitTotal` 为复用绑定账号次数，`affinityMissTotal` 为无有效绑定次数，`affinityFallbackTotal` 为已有绑定但分流次数，`affinitySwitchTotal` 为有效绑定经成功响应切换账号的次数。前三项在每次获得上游名额时计数，包含重试，不是客户端请求数或上游缓存命中数；已有绑定复用率为 `hit / (hit + fallback)`。管理首页展示这些指标，调用详情和日志 JSON 的 `affinity` 字段记录最后一次带会话标识的调度结果（`hit` / `miss` / `fallback`），可以结合首包耗时、上游尝试和 429 记录评估效果；旧日志没有此字段。所有亲和累计指标自进程启动起统计。

启用自适应并发后，管理首页同时显示恢复中、单请求探测中的账号数和实际降档次数，账号列表显示当前生效上限及单请求探测成功后的目标上限。`concurrency.adaptiveConcurrency` 表示是否启用，`concurrency.recoveringCredentials` 统计已启用且尚在探测或尚未恢复配置上限的账号（包括冷却中的账号），`concurrency.probingCredentials` 是其中处于单请求探测的账号数；`concurrency.adaptiveReductionTotal` 只统计容量实际降低的次数，同批请求的迟到 429 不重复计数；`credentials[].effectiveConcurrencyLimit` 是各账号当前容量上限，探测中的账号还会返回 `adaptiveProbeTarget`，冷却仍会暂时阻止调度。

`GET /api/admin/credentials` 同时返回 `concurrency` 对象，包含 `inFlight`、`waiting`、`waitingOpus`、`maxInFlight`、`maxInFlightPerCredential`、`maxWaiting`、`waitTimeoutSeconds`、`coolingCredentials`、`saturatedCredentials`、`probingCredentials`、`oldestWaitMs`；累计指标为 `admittedTotal`、`queuedTotal`、`queueFullTotal`、`waitTimeoutTotal`、`cancelledWaitTotal`、`rateLimitedTotal`、`adaptiveReductionTotal`、`finishedWaitTotal`、`totalWaitMs`、`maxWaitMs`、`p50WaitMs`、`p95WaitMs`。`waitingOpus` 仅统计当前等待中的 Opus 请求，其他等待数为 `waiting - waitingOpus`。`credentials[]` 新增 `inFlight` 和 `cooldownRemainingMs`。这些指标只针对当前进程，自启动起累计且不持久化；获准/入队次数按上游名额获取计数，不代表客户端请求数或成功次数。排队耗时包含已离队的成功、错误、超时和取消等待，不包含仍在队列中的调用；均值用 `totalWaitMs / finishedWaitTotal` 计算。P50/P95 由固定桶估算（边界 10/25/50/100/250/500/1000/2500/5000 毫秒），超过 5 秒的分位数以观测最大值表示。全局在途数包含已删除账号尚未结束的调用。

### 调用日志与错误查询

进入 `/admin`，登录后点击右上角 **调用日志**。页面提供时间范围、完整模型名、调用状态、凭据 ID、HTTP 状态码和关键字查询，支持分页、每 10 秒刷新及单条详情 JSON 导出。关键字可搜索请求 ID、接口路径、上游错误和诊断信息；凭据 ID 匹配任一上游尝试，包括故障转移前使用的凭据。列表展示最后使用的凭据。统计卡片按全部查询结果汇总，而不是只计算当前页。

- **原有运行日志**：输出到服务标准输出，可在启动终端查看；Docker 使用 `docker logs --tail 200 kiro-rs` 或 `docker logs -f kiro-rs`。`RUST_LOG` 控制终端日志级别。
- **原有凭据统计**：凭据目录下的 `kiro_stats.json` 只保存成功次数和最后使用时间，不是调用明细。
- **新增调用明细**：默认写入凭据文件同目录的 `kiro_requests.sqlite3`（伴随 SQLite 的 `-wal`、`-shm` 文件）。Docker 默认位于 `/app/config/kiro_requests.sqlite3`，随现有 `./config/` 挂载持久保存。可通过 `requestLogPath` 改路径；自定义到容器其他目录时也应挂载该目录。

记录覆盖 `/v1/*`、`/cc/v1/*` API，包括认证失败、请求校验失败、普通对话、WebSearch、压缩、模型查询和 Token 计数。响应中的 `x-request-id` 对应日志详情 ID。管理 API 与静态资源访问不进入调用日志。

每条记录包含时间、请求 ID、接口、模型、是否流式、HTTP 状态、总耗时、首个响应数据块耗时、输入/输出 Token、凭据 ID、上游调用次数、上游请求 ID及错误详情。详情展示各次调用的上游 HTTP 状态和错误响应，以及该请求中的警告、错误和底层错误链（如 DNS、连接、TLS、超时、帧校验失败）。`RUST_LOG=off` 不影响调用明细和诊断的采集。

普通对话和压缩请求的每次上游响应还会保存 `upstreamAttempts[].responseDiagnostics`：响应类型和声明长度、实际接收字节数、数据块数、事件类型计数、各类事件首帧的字段路径、文本/非空白文本/推理/工具参数的字节数，以及不完整帧和未完成工具调用数量。可据此区分零字节、仅元数据、空白文本、未知事件、解码失败和传输中断。HTTP 200 后的响应体错误也会回填到对应调用的 `error`，每次重试单独保留；失败诊断同时输出到终端。字段路径只采样对象结构，最多 32 类事件（另有溢出汇总项）、每类 32 个字段路径、深度 3 层，不保存字段值。更新后的新调用才有这些数据，历史记录仍可读取。

调用状态分为成功、失败、中断、进行中。SSE 返回 HTTP 200 后收到 `error` 事件仍记为失败；流未完整结束、客户端取消或断开记为中断。服务重启后，未结束的记录标为中断、结果未知，避免把未完成请求算作成功。流式调用的用量仅包含已收到的 usage；未取得用量时显示 `—`，不会编造为零。Token 使用服务已有计数/估算；压缩请求累计 `usage.iterations`，`count_tokens` 的计数结果也纳入输入统计，这些数字不等同于上游账单。

详情还保存最终响应的 `response.content`、`response.stopReason`（客户端的 `stop_reason`）及 `response.truncated`。流式响应合并 content 块和增量，工具参数尽量还原为 JSON；未完整接收或被截断的参数保留为字符串预览。预览中的字符串和字段名共用 16 KiB 预算，另限制结构节点、嵌套深度和数组长度，超出会标记截断。每次上游尝试的 `responseDiagnostics.content` 保存最多 4 KiB 的助手文本，`contentTruncated` 表示是否截断，`stopReason` 保存上游 metadata 的原始停止原因；它可能与转换后返回客户端的停止原因不同。管理页直接展示这些字段，导出 JSON 同样包含；列表不携带正文预览。正常短回答不会仅因 Token 少而自动重试。旧记录无法补录这些字段。

保存调用元数据、错误、诊断和上述响应预览，不保存请求对话正文或认证请求头；响应预览及错误中的常见令牌格式、密钥字段和带认证的代理 URL 会脱敏。预览可能包含模型生成的业务内容，查询和导出接口均要求 Admin API Key。每次调用最多采集 256 条诊断，超出时会显示省略提示；上游错误响应和最终错误单独保留。普通 HTTP 错误正文采集上限为 1 MiB，上游错误正文通过 Provider 独立记录。

默认保留 30 天，启动及每 128 次日志写入清理过期记录并裁剪到最近 100000 条；清理间隔内可能短暂超过数量上限。查询会立即排除过期记录。旧版本没有保存的调用无法补录。日志数据库初始化失败时启动会明确报错；运行期间写入失败也会输出到终端。

管理查询 API（沿用 Admin Key 认证）：

```text
GET /api/admin/logs?page=1&pageSize=20&status=error&model=claude-sonnet-4-6
GET /api/admin/logs/{request-id}
```

其他可选参数：`credentialId`、`httpStatus`、`search`、`from`、`to`。时间使用 Unix 毫秒，边界均包含；`pageSize` 最大 100。列表返回 `items`、`summary`、`page`、`pageSize`、`retentionDays`、`maxRows`，详情返回完整上游尝试和诊断记录。

## 注意事项

1. **凭证安全**: 请妥善保管 `credentials.json` 文件，不要提交到版本控制
2. **Token 刷新**: 服务会自动刷新过期的 Token，无需手动干预
3. **WebSearch 工具**: 当 `tools` 列表仅包含一个 `web_search` 工具时，会走内置 WebSearch 转换逻辑。使用最近一条用户消息中的文本作为查询；`stream: false` 返回 JSON，`stream: true` 返回 SSE，两者都包含关联的服务端工具调用和搜索次数。有效的零结果搜索正常返回；上游 HTTP、MCP 协议、工具执行及结果解析错误会返回错误响应，不再伪装为“没有结果”。
4. **请求校验**: `/v1` 和 `/cc/v1` 在普通对话、WebSearch 和压缩分流之前统一检查非空模型、正数 `max_tokens`、非空消息列表，以及消息角色和内容的基本结构；错误返回 `400 invalid_request_error`。`messages` 中的 `system` / `developer` 文本指令会按原顺序追加到顶层 `system`，保留已有系统提示；支持字符串或 text 块数组，转换后仍须包含 user/assistant 消息。未知角色及不合法内容返回带 `messages[i]` 位置的错误，不会静默丢弃；工具结果仍应使用 user 消息中的 `tool_result` 块。`count_tokens` 使用相同的消息规范化和结构校验。`system` 支持字符串、数组和 `null`。`metadata.user_id` 中的会话 ID 仅在符合标准 UUID 格式时复用，否则自动生成新 ID。
5. **上游事件校验**: 收齐 12 字节帧前缀后立即校验 CRC 和头部长度，不等待损坏帧的剩余数据。头部解析严格遵守声明边界，拒绝重复名称和非法 UTF-8。普通响应与压缩摘要均会报告解析错误；JSON 返回 `502 api_error`，SSE 发送错误事件并结束，不发送正常完成事件。
6. **瞬态错误重试**: 对话和 WebSearch 在重试 `408`、`429`、`5xx` 时识别上游 `Retry-After`（秒数或 HTTP 日期），等待时间至少满足该要求及原有指数退避。格式无效时沿用指数退避；要求等待超过 60 秒时停止本次重试并返回错误，不提前重试。退避等待期间释放凭据的并发预留，瞬态错误不计入凭据失败次数。429 还会让对应账号进入至少 `concurrency.rateLimitCooldownSeconds` 的冷却，存在有效 `Retry-After` 时取更长者；其他请求会跳过该账号，并发 429 不会缩短已有冷却。
7. **空回复处理**: 普通 JSON、实时 SSE、`/cc` 缓冲 SSE 和压缩摘要统一检测空响应体、仅 usage/元数据事件及纯空白内容。未收到有效内容且没有明确长度限制时，额外尝试最多 2 次，分别等待 200/400 毫秒；重试时丢弃前一次的空白及 usage。持续为空时，JSON 返回 `502 api_error`，已开始的 SSE 发送 `error` 并结束，不发送正常完成事件。有效 thinking、工具调用及明确的长度/上下文限制不触发空回复重试；发送内容后发生解析、工具完整性或读取错误，也不会重放请求。空压缩摘要不会替换历史。对话成功次数在完整响应校验通过后记录，空回复不计为成功，也不会因此禁用凭据。每次尝试中的 HTTP/网络错误仍使用上一条的重试规则。

## 项目结构

```
kiro-rs/
├── src/
│   ├── main.rs                 # 程序入口
│   ├── http_client.rs          # HTTP 客户端构建
│   ├── token.rs                # Token 计算模块
│   ├── debug.rs                # 调试工具
│   ├── test.rs                 # 测试
│   ├── model/                  # 配置和参数模型
│   │   ├── config.rs           # 应用配置
│   │   └── arg.rs              # 命令行参数
│   ├── anthropic/              # Anthropic API 兼容层
│   │   ├── router.rs           # 路由配置
│   │   ├── handlers.rs         # 请求处理器
│   │   ├── middleware.rs       # 认证中间件
│   │   ├── types.rs            # 类型定义
│   │   ├── converter.rs        # 协议转换器
│   │   ├── stream.rs           # 流式响应处理
│   │   └── websearch.rs        # WebSearch 工具处理
│   ├── kiro/                   # Kiro API 客户端
│   │   ├── provider.rs         # API 提供者
│   │   ├── token_manager.rs    # Token 管理
│   │   ├── machine_id.rs       # 设备指纹生成
│   │   ├── model/              # 数据模型
│   │   │   ├── credentials.rs  # OAuth 凭证
│   │   │   ├── events/         # 响应事件类型
│   │   │   ├── requests/       # 请求类型
│   │   │   ├── common/         # 共享类型
│   │   │   ├── token_refresh.rs # Token 刷新模型
│   │   │   └── usage_limits.rs # 使用额度模型
│   │   └── parser/             # AWS Event Stream 解析器
│   │       ├── decoder.rs      # 流式解码器
│   │       ├── frame.rs        # 帧解析
│   │       ├── header.rs       # 头部解析
│   │       ├── error.rs        # 错误类型
│   │       └── crc.rs          # CRC 校验
│   ├── admin/                  # Admin API 模块
│   │   ├── router.rs           # 路由配置
│   │   ├── handlers.rs         # 请求处理器
│   │   ├── service.rs          # 业务逻辑服务
│   │   ├── types.rs            # 类型定义
│   │   ├── middleware.rs       # 认证中间件
│   │   └── error.rs            # 错误处理
│   ├── admin_ui/               # Admin UI 静态文件嵌入
│   │   └── router.rs           # 静态文件路由
│   └── common/                 # 公共模块
│       └── auth.rs             # 认证工具函数
├── admin-ui/                   # Admin UI 前端工程（构建产物会嵌入二进制）
├── tools/                      # 辅助工具
├── Cargo.toml                  # 项目配置
├── config.example.json         # 配置示例
├── docker-compose.yml          # Docker Compose 配置
└── Dockerfile                  # Docker 构建文件
```

## 技术栈

- **Web 框架**: [Axum](https://github.com/tokio-rs/axum) 0.8
- **异步运行时**: [Tokio](https://tokio.rs/)
- **HTTP 客户端**: [Reqwest](https://github.com/seanmonstar/reqwest)
- **序列化**: [Serde](https://serde.rs/)
- **日志**: [tracing](https://github.com/tokio-rs/tracing)
- **命令行**: [Clap](https://github.com/clap-rs/clap)

## License

MIT

## 致谢

本项目的实现离不开前辈的努力:  
 - [kiro2api](https://github.com/caidaoli/kiro2api)
 - [proxycast](https://github.com/aiclientproxy/proxycast)

本项目部分逻辑参考了以上的项目, 再次由衷的感谢!

## 常见排错与经验 (Troubleshooting)

### 1. IdC 认证报 ă Forbidden: User is not authorized to make this call.如果你在使用 IdC (Enterprise) 认证时，代理能正常启动并刷新 Token，但在实际发送对话请求时 Kiro 后端返回了 403 权限错误，这通常是因为**配置文件中漏掉了 \profileArn\ 字段**。
- **原因**：企业版 IdC (AWS Identity Center) 在调用 Kiro API 时，强制要求在请求中附带 \profileArn\ 标识身份环境。
- **解决方案**：在 \credentials.json\ 中补齐 \profileArn\ 字段。如果你是用官方的 Kiro IDE 插件登录的，可以在本地缓存中找到它（例如 Windows 下通常位于 \%APPDATA%\Kiro\User\globalStorage\kiro.kiroagent\profile.json\ 中）。

### 2. 启动时报 \没有可用的 accessToken\ 并导致凭据被禁用
当手动从缓存（如 \kiro-auth-token.json\）拼接生成 \credentials.json\ 时，如果你保留了未来的 \expiresAt\ 时间，但忘记配置 \ccessToken\，就会报这个错。
- **原因**：当代理服务发现 \expiresAt\ 是未来时间时，它会认为当前 Token 依然有效，于是**跳过网络刷新流程**，直接尝试读取 \ccessToken\ 去发请求。如果此时配置里没有 \ccessToken\，就会因找不到 Token 而报错并禁用该凭据。
- **解决方案**：
  - **方法一**：将 \ccessToken\ 完整写入 \credentials.json\ 中。
  - **方法二**：删除 \expiresAt\ 字段，或者将其改为过去的时间。这样代理服务在启动时就会强制触发一次 Token 刷新，自动获取新的 \ccessToken\。
