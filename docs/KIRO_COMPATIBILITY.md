# Kiro 兼容性记录（2026-11-06）

核对依据：Kiro 官方更新日志、模型文档，以及本机安装的 Kiro IDE 1.4.86（代理扩展 1.1.718）的请求构造和事件 schema。客户端代码可以确认字段与调用方式；本次未使用真实账号调用上游，账号、区域、组织授权及服务端模型 schema 仍需实际验证。

## 本次适配

| 项目 | 行为 |
|------|------|
| 客户端版本标识 | 默认 Kiro 版本更新为 1.4.86，Streaming SDK 标识更新为 1.0.44；显式 `kiroVersion` 仍优先 |
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

## 已核对但未接入的范围

| 新特性 | 对本项目的影响 |
|--------|----------------|
| GPT-5.6 Sol / Terra / Luna、Kimi K3 | 官方已正式提供，但当前代理的模型路由仍只接受 Claude。本次未将新增名称简单加入模型列表；需要结合账号的 `ListAvailableModels` 验证模型 ID、授权、区域及 schema，再实现对应推理参数。GPT 使用 `reasoning.effort`，不能套用 Claude 的 `output_config.effort` |
| 动态模型发现 | 新 IDE 从 `ListAvailableModels` 获取 `additionalModelRequestFieldsSchema`、输入上限、effort 级别与默认值；当前代理继续使用本地映射，后续新增模型仍需更新代码 |
| Runtime Responses 接口 | 新 IDE 除 `generateAssistantResponse` 外，还包含受功能开关控制的 `/v1/responses` 路径。本次保留现有 AWS Event Stream 通路，未声明支持新的 SSE 协议 |
| CLI V3、Workflows、Memory、审批及 MCP elicitation | 属于 Kiro agent harness 和工具执行层。本项目提供模型 API 代理，未实现这些客户端工作流功能 |
| 企业 credit pooling | 属于订阅及组织额度管理；现有 credits 日志不能替代共享额度池和成员级上限管理 |

## 参考资料

- [官方更新日志](https://kiro.dev/changelog/)：11 月 3 日 CLI 3.0、11 月 4 日 Haiku 5.5 / Kimi K3 正式可用及 credit pooling。
- [模型列表与地区、上下文](https://kiro.dev/docs/models.md)。
- [模型能力与生命周期](https://kiro.dev/docs/models/available-models.md)。
- [Reasoning effort、thinking schema 与输出上限](https://kiro.dev/docs/models/effort.md)。其中 Sonnet 5.5 的 `between_tools` effort 限制依据该页 Configuring thinking behavior 段落；表格列出的是模型整体的 effort 集合。
- 本机静态核对文件：`/Applications/Kiro.app/Contents/Resources/app/product.json`、`extensions/kiro.kiro-agent/package.json` 及 `dist/extension.js`。未读取登录缓存或令牌。
