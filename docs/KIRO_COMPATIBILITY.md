# Kiro 兼容性记录（2026-11-06）

核对依据：Kiro 官方更新日志、模型文档，以及本机安装的 Kiro IDE 1.4.86（代理扩展 1.1.718）的请求构造和事件 schema。客户端代码可以确认字段与调用方式；本次未使用真实账号调用上游，账号、区域、组织授权及服务端模型 schema 仍需实际验证。

## 本次适配

| 项目 | 行为 |
|------|------|
| 客户端版本标识 | 默认 Kiro 版本更新为官方稳定版 1.4.121（2026-11-06），Streaming SDK 标识更新为 1.0.44；显式 `kiroVersion` 仍优先 |
| 原生生成上限 | 启用原生模式后传递 `additionalModelRequestFields.max_tokens`；旧协议模式仍不发送原生字段 |
| 新版 Claude thinking | 原生模式将支持 adaptive 的模型的 `enabled` 转为 `adaptive`，移除固定预算；4.5 系列保留 budgeted thinking |
| Sonnet 5.5 | 支持 `between_tools`；兼容 `disabled` 请求并转为 `between_tools`，该设置仅接受 low / medium / high effort |
| 推理显示设置 | 接受、校验并在原生模式传递 `thinking.display: summarized / omitted` |
| 模型能力与上限 | 修正 Haiku 4.5 的 budgeted thinking 能力；模型列表与详情共用输出上限，Sonnet 5.5 为 64K |
| 用量 metadata | 优先使用 `metadataEvent.tokenUsage` 的有效 token 计数，覆盖 JSON、实时 SSE、缓冲 SSE、压缩摘要；无上报时保留既有估算 |
| 缓存计数 | `cacheReadInputTokens` / `cacheWriteInputTokens` 转为 Anthropic 缓存用量字段；压缩调用保存在 `usage.iterations` 中 |

原生模式沿用配置入口（保存后重启）：

```json
{
  "endpoints": {
    "ide": {
      "nativeThinking": true
    }
  }
}
```

该开关控制原生模型参数及 reasoning 历史传输。默认仍是旧协议模式；旧协议中的 thinking/effort 使用提示词引导，`thinking.display` 和原生 `max_tokens` 仅在原生模式传输。启用后，上游按所选模型 schema 校验生成上限；不自动夹取请求值或改变账号、区域。模型能力列表是本地兼容表，不代表当前凭据实际拥有对应授权。

用量按单次生成快照更新，缺失字段不覆盖已有数值，重复事件不累加。明确上报 0 时保留 0；负数、小数、溢出及非数字忽略。`uncachedInputTokens` 对应 Anthropic `input_tokens`，缓存读写单列；不使用 `totalTokens` 或 `normalizedTokenUsage` 替代。实时 SSE 的 `message_start` 仍先返回估值，最终用量在 `message_delta`；缓冲 SSE 会更正 `message_start` 中的输入及缓存数。已有 Kiro credits 计费记录继续取自 `meteringEvent`，与 token 计数分开。

## 版本检查与一键更新

系统设置 → **Kiro 版本** 可查看运行版本、已保存版本与官方最新稳定版。“检查最新版本”读取官方公开更新源；“一键更新版本配置”会重新检查版本，仅在版本更高时保存 `kiroVersion`，重启服务后生效。手动指定版本并保存可回退。存在未保存修改时需先保存或撤销，再使用一键更新。

检查 API：`GET /api/admin/settings/kiro-version`，需要管理员认证，返回 `latestVersion`、`publishedAt`、`checkedAt` 与 `sourceUrl`。版本标识与操作系统无关，固定读取 [官方 Linux x64 稳定版元数据](https://prod.download.desktop.kiro.dev/stable/metadata-linux-x64-stable.json)，本次同时核对 macOS ARM64、Windows x64 均为 1.4.121。检查使用运行中的全局代理与 TLS 配置，有超时和响应大小限制；失败时不回写默认版本。

保存沿用 `PUT /api/admin/settings` 的版本冲突校验和配置持久化，保留密钥与未纳入管理的字段。不下载或安装 Kiro IDE，也不替换 kiro-rs 程序。升级版本标识不等于实现新模型或新协议；协议适配依据仍为上述本机 1.4.86 的静态代码，1.4.121 尚未进行真实账号调用验证。

## 按账号发现可用模型

凭据管理 → 账号右侧更多操作 → **查看可用模型**，查询该账号在有效 API 区域中的上游模型目录。支持搜索名称、ID、提供商，展示账号默认模型、输入/输出 token 上限、计费倍率、effort 枚举及默认值、thinking 类型及默认值；上游未提供的参数标记为“未上报”。

接口为 `GET /api/admin/credentials/{id}/models`，`?force=true` 强制刷新，需要管理员认证。沿用凭据自身的 API 区域、Profile ARN、代理与托管 OAuth 刷新；API Key 直接使用密钥，IdC 请求带 `SSO_OIDC` token 类型。端点依据本机新版 IDE 控制平面代码：`https://management.<apiRegion>.kiro.dev/List-Available-Models?origin=AI_EDITOR`。查询仅支持 IDE 凭据，未对真实账号发起验证。

缓存仅驻留当前进程，按账号身份、Profile ARN、区域和端点隔离，5 分钟内复用，最多保存 128 个账号；每个账号的并发查询共用一次请求，全局最多 4 次目录查询。分页最多 10 页、1000 个模型、合计 4 MiB，单页最多 1 MiB；游标重复、分页中途失败、缺少 models 等情况整次查询失败，不把部分结果当成完整授权表。失败时最多保留 1 小时内同一账号的旧记录，返回 `stale: true` 和 `refreshError`；旧记录不延长新鲜期，也不能确认当前授权。删除账号时清除其缓存。

目录从 `additionalModelRequestFieldsSchema` 提取参数，不向管理接口返回原始 schema、令牌、Profile ARN、分页游标或上游错误正文。`proxySupported` 仅在上游 ID 精确匹配现有代理映射时为 true，避免未知 Claude 新版本被宽松别名匹配误标为已适配。GPT/Kimi 可在账号目录出现并查看参数，但显示“代理待适配”。普通 `/v1/models` 仍是本地兼容表，新模型的生成仍需代码适配。

## 按模型权限选择账号

管理页或上述管理 API 成功查询后，完整目录立即参与生成请求的账号选择，原始获取时间起 5 分钟有效。读取缓存、关闭重开弹窗不会延长有效期；当前进程重启后需重新查询。生成路径仅访问内存，不自动发起目录查询。

- 目录明确包含请求实际发送的模型 ID 时，允许该账号，即使旧订阅名称仍标为 FREE；目录明确缺失模型时跳过该账号，即使订阅为 PRO。完整空目录不允许生成，但不影响不指定模型的 MCP 工具调用。
- 优先选择有新鲜目录且确认支持模型的可调度账号，再按会话亲和、负载均衡或优先级选择。确认支持的账号满载或冷却时，可使用权限未知且通过既有订阅规则的账号。保持请求原本模型，不自动降级模型。
- 未查询、过期、刷新失败或账号认证、Profile ARN、API 区域、端点变化时，不使用旧目录判断授权，恢复既有订阅规则。失败时展示的旧目录仅供查看；迟到的缓存响应不能恢复已标记失败的同一快照。
- 排队按完整模型区分账号资格，同属 Sonnet/Opus 的不同版本也分别判断；不可调度的旧请求不占用其他模型可用的账号，已可调度的旧请求保留到达顺序。目录更新会唤醒等待者重新检查资格，目录到期也会触发重新检查。

此功能只影响账号选择，不修改原生参数 schema、已在途请求或 `/v1/models`，也不把模型未授权计为账号故障。

## 后续适配范围

| 新特性 | 对本项目的影响 |
|--------|----------------|
| GPT-5.6 Sol / Terra / Luna、Kimi K3 | 已能在账号目录发现并展示参数；生成路由仍只接受 Claude，需继续实现非 Claude 推理参数和生成协议。GPT 使用 `reasoning.effort`，不能套用 Claude 的 `output_config.effort` |
| 动态模型路由与 schema 校验 | 已实现按账号目录展示及新鲜目录驱动的账号资格判断；`/v1/models` 与请求参数校验仍使用本地兼容表，后续新增生成模型仍需更新代码 |
| Runtime Responses 接口 | 新 IDE 除 `generateAssistantResponse` 外，还包含受功能开关控制的 `/v1/responses` 路径。本次保留现有 AWS Event Stream 通路，未声明支持新的 SSE 协议 |
| CLI V3、Workflows、Memory、审批及 MCP elicitation | 属于 Kiro agent harness 和工具执行层。本项目提供模型 API 代理，未实现这些客户端工作流功能 |
| 企业 credit pooling | 属于订阅及组织额度管理；现有 credits 日志不能替代共享额度池和成员级上限管理 |

## 参考资料

- [官方更新日志](https://kiro.dev/changelog/)：11 月 3 日 CLI 3.0、11 月 4 日 Haiku 5.5 / Kimi K3 正式可用及 credit pooling。
- [模型列表与地区、上下文](https://kiro.dev/docs/models.md)。
- [模型能力与生命周期](https://kiro.dev/docs/models/available-models.md)。
- [Reasoning effort、thinking schema 与输出上限](https://kiro.dev/docs/models/effort.md)。其中 Sonnet 5.5 的 `between_tools` effort 限制依据该页 Configuring thinking behavior 段落；表格列出的是模型整体的 effort 集合。
- 本机静态核对文件：`/Applications/Kiro.app/Contents/Resources/app/product.json`、`extensions/kiro.kiro-agent/package.json` 及 `dist/extension.js`。未读取登录缓存或令牌。
