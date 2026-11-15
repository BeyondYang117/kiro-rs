# 真实 Kiro 账号联调

`src/anthropic/live_tests.rs` 使用生产 Token 管理器、CLI / IDE 端点、账号目录、余额查询和完整 Messages 路由。普通 `cargo test --all-targets` 会忽略付费联调，只运行选择逻辑及响应验证器回归；显式 `--ignored` 才会连接 Kiro。

## 使用文件中的账号

```sh
KIRO_LIVE_CREDENTIALS=/absolute/path/credentials.json \
KIRO_LIVE_CONFIG=/absolute/path/config.json \
KIRO_LIVE_ACCOUNT_ID=1 \
cargo test live_account_smoke -- --ignored --nocapture
```

`KIRO_LIVE_CONFIG` 可省略，沿用默认区域 `us-east-1` 和 CLI 端点。文件有多个启用账号时必须指定 `KIRO_LIVE_ACCOUNT_ID`；单个账号无需 ID。禁用账号不能被测试选中，测试不进行跨账号故障转移。凭据文件可使用本项目单对象 / 数组格式，也可直接指向包含 `accessToken`、`refreshToken`、`expiresAt`、`authMethod`、`profileArn` 的本机 Kiro 登录缓存；测试不会自动发现或使用缓存。

也可以在本机环境中设置 `KIRO_API_KEY` 后运行测试，不同时设置 `KIRO_LIVE_CREDENTIALS`。不要将密钥写进命令、提交到 Git 或粘贴到报告中。

## 模型和端点矩阵

可选环境变量：

| 变量 | 行为 |
|------|------|
| `KIRO_LIVE_MODELS` | 1–4 个逗号分隔的精确上游 ID；必须在该账号新鲜目录中，且代理已适配 |
| `KIRO_LIVE_ENDPOINT` | `cli` 或 `ide`，仅覆盖本次测试内存中的端点 |
| `KIRO_LIVE_NATIVE_THINKING` | `true` / `false`，仅覆盖本次端点的 Claude 原生参数开关 |

默认从账号目录按计费倍率为 Claude、GPT、Kimi 各选一个已适配模型；目录未提供的系列不会被测试，也不代表它们已通过。需要全面覆盖时显式指定模型，例如：

```sh
KIRO_LIVE_CREDENTIALS=/absolute/path/credentials.json \
KIRO_LIVE_MODELS=gpt-5.6-sol,gpt-5.6-terra,gpt-5.6-luna,kimi-k3 \
cargo test live_account_smoke -- --ignored --nocapture
```

每次运行先查询真实模型目录和余额，每个模型最多执行 6 个生成请求：

1. `/v1/messages` JSON 文本响应。
2. `/v1/messages` 实时 SSE。
3. `/cc/v1/messages` 实时 SSE。
4. `/cc/v1/messages` 缓冲 SSE。
5. JSON 工具调用，检查名称、完整参数、ID、`tool_use` stop reason。
6. `/cc/v1/messages` 工具结果继续对话，验证 `end_turn` 和固定响应标记。

工具只回传固定字符串，不执行文件、Shell、网络或其他外部操作。GPT 请求使用账号 schema 确认的 `reasoning.effort`，优先 `low` / `none`。每次生成只允许一次上游尝试，关闭工具输出恢复；请求的 `max_tokens` 为目录确认的最小值与 1024 的较大者，超过 4096 则停止。若账号 schema 不接受该字段（本次实测 GPT / Kimi 如此），传输层会省略它，该值不能限制上游实际输出；测试使用短提示、低 effort、一次尝试和响应体/时间限制控制开销。单请求总超时 120 秒，测试读取响应最大 1 MiB。JSON / SSE 均校验用量类型、完成状态；SSE 还校验块生命周期、事件顺序和最终停止事件。失败不会自动重试整个矩阵。

测试只在内存管理账号，不启动监听服务、不修改配置和源凭据、不保存统计或请求数据库。OAuth 到期时仍会向官方服务刷新令牌；若认证服务实行 refresh token 轮换，需要重新登录或更新凭据后再运行，测试不负责向登录缓存回写。不要与同一账号的主动刷新操作并行运行。

默认输出仅含端点、区域、目录模型数量、模型 ID 和通过状态；不输出令牌、Profile ARN、邮箱、余额值或原始生成内容。请勿开启额外的 HTTP trace。未完成整个矩阵时，报告应列明已通过部分和失败阶段，不得写作全面联调通过。


## additionalProperties 原样上游验证

2026-11-15 使用一个明确导入的账号，在 CLI / `us-east-1` / `claude-haiku-5.5` 上执行 9 次独立生成请求。根层与 `properties.capabilities` 层各测试未设置、`true`、`false`、`{"type":"string"}`；另测不预声明属性、只用 schema 值定义字典的 `capabilities`。9 次均 HTTP 200、无 event-stream 异常，完整返回 `{"capabilities":{"read":"KIRO_SCHEMA_OK"}}` 工具参数。没有执行工具，也没有自动重试。

探针 `live_raw_additional_properties_probe` 直接构造 reqwest Request，复用生产端点鉴权、请求体转换和模型参数处理，明确绕过 provider 的工具 schema 适配函数。它在构建 HTTP Request 后，从实际 body 字节反序列化，断言第二个工具的整个 schema 与输入相等，计算 SHA-256，然后执行同一个 Request。测试不经本地 Messages 路由，因此不会被入口校验或 schema 清理影响。每例 schema、请求体哈希、上游 request ID、HTTP 状态、事件名称和工具参数保存在[脱敏结果](../testdata/kiro_additional_properties_live_2026-11-15.json)中；不保存 Token、请求头、Profile ARN 或完整账号请求体。

修改并重启服务后，另通过本地 `/v1/messages` 走完整生产链路发送纯字典定义，真实上游返回 HTTP 200、`stop_reason=tool_use` 和预期参数；该生产链路补验独立于上述 9 次原始探针请求。最终全量离线测试 620 项通过、2 项付费联调默认跳过。

结论：该账号、区域、端点和模型接受布尔值及 schema 值的 `additionalProperties`，原先“该关键字一律不受支持”的假设不成立。代理改为保留该关键字，不再删除或仅因其是 schema 对象而拒绝请求。这些测试使用合法参数，**没有证明上游严格执行所有 JSON Schema 约束**，也不能外推至其他端点、模型或区域。

复测（9 次生成会消耗额度）：

```sh
KIRO_LIVE_CREDENTIALS=/absolute/path/credentials.json \
KIRO_LIVE_CONFIG=/absolute/path/config.json \
KIRO_LIVE_ACCOUNT_ID=1 \
KIRO_SCHEMA_PROBE_OUTPUT=/absolute/path/probe-output \
cargo test live_raw_additional_properties_probe -- --ignored --nocapture
```

可设置 `KIRO_SCHEMA_PROBE_CASES=dictionary-schema` 只复测纯字典；其他标签为 `root-absent`、`root-true`、`root-false`、`root-schema` 及对应的 `nested-*`。输出目录仅保存合成工具 schema 和脱敏测试结果。未成功完成 HTTP 请求、事件流或预期工具参数的用例会导致测试失败，结果文件仍保留已完成用例。


## additionalProperties 违规参数验证

2026-11-15 继续使用同一账号、CLI / `us-east-1` / `claude-haiku-5.5`，通过原始上游探针执行 3 组对照、共 6 次请求。每组提示相同，只改变 `additionalProperties`：控制组为 `true`，限制组为 `false` 或 `{"type":"string"}`。提示明确要求模型原样生成指定参数，包括故意与 schema 冲突的键或类型。探针断言实际 HTTP body 中整个工具 schema 未被改写，绕过本地响应校验，按 tool call ID 拼接原始参数并记录完成标记；没有执行工具、没有重试。6 次均 HTTP 200，event-stream 无错误或异常，工具调用完整结束。

| 用例 | `true` 控制组 | 限制组原始工具参数 | 观察 |
|---|---|---|---|
| 根层禁止额外字段 | 返回 `unexpected` | `{"value":"KIRO_SCHEMA_OK"}` | 本次省略了额外字段，无法区分模型遵守与服务端强制 |
| 嵌套层禁止额外字段 | 返回 `unexpected` | `{"capabilities":{"read":"KIRO_SCHEMA_OK","unexpected":"EXTRA_KEY"}}` | **违反** `capabilities.additionalProperties:false` |
| 字典值必须为字符串 | 返回数字 `123` | `{"capabilities":{"read":123}}` | **违反** `capabilities.additionalProperties:{"type":"string"}` |

结论：在该测试配置中，Kiro 接受 schema，但**不能保证严格执行 `additionalProperties` 约束**。后两项存在完整的违规工具调用反例；根层单次合规输出不能证明硬约束。该结果不外推至其他模型、端点、区域或未测试的严格生成模式。测试通过仅表示成功记录完整实验，并不表示输出均符合 schema。

[脱敏原始记录](../testdata/kiro_additional_properties_enforcement_live_2026-11-15.json)包含提示、schema、原始工具参数字符串、解析后的参数、违规路径、上游 request ID 和请求体哈希。生产逻辑继续保留 schema。后续按用户要求移除了入口 schema 修正/矛盾检查、根层组合包装，以及返回参数的 `required`/`additionalProperties:false` 本地检查；保留 JSON、调用 ID、帧与截断等协议完整性检查。

复测（6 次生成）：

```sh
KIRO_LIVE_CREDENTIALS=/absolute/path/credentials.json \
KIRO_LIVE_CONFIG=/absolute/path/config.json \
KIRO_LIVE_ACCOUNT_ID=1 \
KIRO_SCHEMA_PROBE_OUTPUT=/absolute/path/enforcement-output \
cargo test live_raw_additional_properties_enforcement_probe -- --ignored --nocapture
```

可用 `KIRO_SCHEMA_PROBE_CASES` 选择用例，标签为 `root-extra-control`、`root-extra-restricted`、`nested-extra-control`、`nested-extra-restricted`、`dictionary-type-control`、`dictionary-type-restricted`，多个标签用逗号分隔。


## 工具 schema 透传审查

审查 converter、provider、CLI/IDE 端点和共享响应解码器后，移除以下推测性 schema 行为：

- 补写缺失字段、将根层 `type` 数组改成 object、替换 `properties/required`、过滤 `required` 数组内容。
- 递归删除空 `required`、将根层组合 schema 搬入 `$defs` 并改写文档内引用。
- 入口拒绝 `type` 与 `enum/const` 冲突的 schema；这类定义也交给上游处理。
- 返回参数的顶层必填字段检查与递归禁止额外字段检查。完整 JSON 参数保留上游原值。

完整生产路径回归覆盖 `/v1`、`/cc/v1` 的 JSON、实时 SSE 和缓冲 SSE：实际模拟上游收到的 schema 与输入相等；返回参数缺字段、多字段或类型不符时也不被代理拒绝。仍保留工具名映射、历史调用 ID 配对和工具类型分流等协议转换，以及 JSON/帧/结束标记/截断检查。

工具描述截断、采样参数和 `stop_sequences` 省略、强制 `tool_choice` 拒绝已继续做真实上游验证，见下节。账号模型参数限制、跨模型 reasoning 历史清理是其他独立兼容逻辑，本轮未对它们增加新限制，也不能把它们视为已确认适用于所有 Kiro 模型的限制。


## 工具描述与生成控制实测

2026-11-15，使用用户明确导入的一个账号，在 CLI / us-east-1 / `claude-haiku-5.5` 发送 **28 次**原始请求。测试复用生产认证和端点，但在端点/账号参数转换完成后注入待测字段，再核对实际 reqwest 请求体，直接发送到真实 Kiro；绕过入口参数省略、provider 限制及返回 schema 校验。每个用例仅请求一次，无重试，不执行返回的工具。22 次 HTTP 200，6 次 HTTP 400。

| 测试 | 实际结果 | 可得结论 |
|------|----------|----------|
| 10000 / 12000 / 20000 字符工具描述，末尾给出参数标记 | 全部 200，完整工具调用的参数均为末尾标记 `DESC_TAIL_OK` | 10000 字符本地截断没有依据；移除截断和描述后缀，完整保留调用方描述 |
| `additionalModelRequestFields` 中的 temperature、top_p、top_k、stop_sequences、tool_choice、toolChoice | 6 次均 400，明确指出字段未定义且 schema 禁止额外属性 | 该模型当前原生参数位置不接受这些字段 |
| 根层 temperature / top_p / top_k，另测 temperature 字符串及未知哨兵字段 | 全部 200 | 接受不代表支持；未做采样统计，不能证明有无采样效果 |
| 根层 stop_sequences，以及根层/上下文 inferenceConfig.stopSequences | 均返回完整 `BEFORE_STOP_MARKER_AFTER`，没有在 `_STOP_MARKER_` 停止 | 这些候选位置未实现所请求的停止约束 |
| 根层 tool_choice:any / tool，根层/上下文 toolChoice:any、toolConfig.toolChoice:any | 均返回文本，没有工具调用；无效 tool_choice / toolChoice 也 200 | 这些候选位置未实现强制工具调用 |
| 根层 tool_choice:none，提示要求调用工具 | 返回一个完整工具调用 | 原样发送该字段不能实现 none；代理继续通过移除工具定义实现 |
| 根层 disable_parallel_tool_use:true，提示要求两次调用 | 返回两个完整工具调用 | 记录未保存逐帧调用 ID 时序，不能由调用数量断言是否并行；尚无已验证映射 |

同次读取真实账号 `ListAvailableModels` 目录，Haiku 5.5 的模型参数 schema 仅声明 `thinking`、`output_config`、`max_tokens`，根层 `additionalProperties:false`。这与原生字段 400 相互印证，**不是所有模型或所有可能位置均不支持的证明**。

本节验证后的调整：工具描述不再截断或追加内容；当时保留的系统分块提示已在下一轮真实对照后移除，见下节。采样/停止参数继续省略并给出“代理尚无已验证映射”的警告，强制 `tool_choice` 继续明确拒绝，避免成功响应掩盖未实现的调用约束；没有新增 schema 或工具参数校验。完整路由回归验证 20000 个中文字符加末尾标记经过 JSON、实时 SSE、缓冲 SSE 后保持原值，CLI 原生参数开关两种模式的最终请求也保留完整描述。

[脱敏原始记录](../testdata/kiro_tool_controls_live_2026-11-15.json)包含所有候选字段、注入位置、提示、描述哈希及尾部、实际请求体哈希、上游 request ID、HTTP 错误和完整工具参数。结果仅适用于该账号、日期、模型、区域和端点；未验证超过 20000 字符的描述，也没有根据 HTTP 200 声称采样参数已生效。

复测（默认最多 28 次请求，会消耗额度）：

```sh
KIRO_LIVE_CREDENTIALS=/absolute/path/credentials.json \
KIRO_LIVE_CONFIG=/absolute/path/config.json \
KIRO_LIVE_ACCOUNT_ID=1 \
KIRO_CONTROLS_PROBE_OUTPUT=/absolute/path/controls-output \
cargo test live_raw_tool_controls_probe --no-default-features -- --ignored --nocapture
```

可用 `KIRO_CONTROLS_PROBE_CASES` 选择逗号分隔的用例标签，例如 `description-20000,native-temperature,root-tool_choice-any`；完整标签见原始记录中的 `case`。探测会记录 400，而不把 400 视为测试失败；遇到 401/402/403/429 则停止。该测试的通过表示记录完成，不代表待测能力被支持。


## 系统分块提示与输出预算实测

2026-11-15，继续使用同一导入账号、CLI / us-east-1 / `claude-haiku-5.5` 做 **8 次**原始上游对照，不执行任何文件操作。待调用的 `write_file` 只是记录工具参数。比较实际发送的系统历史与预期相等，模型参数在端点和账号转换后注入并核对，保存实际请求体哈希。每组新的会话使用同一任务，只有系统提示不同；所有请求均 200，是否完成由原始工具事件与 JSON 参数核验，不能仅看 HTTP 状态。

来源追溯：50 行源于本地提交 `f25d986`（2026-02-07）的 Write/Edit 分块提示；12000 字符与整次响应 `min(max_tokens,4096)` 软预算源于 `5b15337`（2026-11-04）的截断恢复优化。相关提交没有附官方上游依据或可复查的真实边界测试；它们是本地保守经验值。

| 任务/控制 | 不带原提示 | 带原提示或控制 |
|-----------|------------|----------------|
| 复制 80 行、720 字符，一次工具调用 | 完整返回 80 行，逐字一致 | 原提示只返回前 40 行，解释中明确引用 50 行限制 |
| 复制 550 行、30250 字符，一次工具调用 | 完整返回 550 行，逐字一致，stopReason=`TOOL_USE` | 原提示只返回前 100 行 |
| 同一 550 行任务，仅加入 4096 soft prompt；原生预算仍为 16384 | 无提示完整返回，见上一行 | 仅软预算提示也使它只返回前 50 行 |
| 同一 550 行任务，无软提示，原生 `max_tokens` 从 16384 改为 4096 | 16384 时完整返回 | 4096 时 `MAX_TOKENS`，工具 JSON 未完成 |
| 14000 字符的连续 `abcd` 重复串，原生预算 16384 | `MAX_TOKENS`，工具 JSON 未完成 | 原提示不调用工具，文本引用 12000 字符限制并要求拆分 |

除原生 4096 对照外，实际发送 `additionalModelRequestFields={max_tokens:16384,thinking:{type:"disabled"}}`。上游 metadata 仅报告停止原因，**没有精确 token 用量**，因此这里不把字符数、本地估算或生成任务名称当成真实输出 token 数。

结果说明：50 行和 12000 字符不是该模型所有工具调用的硬边界，550 行/30250 字符的完整反例已超过它们；原提示和单独 4096 软预算会无谓改变任务完成范围。重复串用例同时证明真实长度失败仍可能发生，不能据此保证任意长输出成功，也不能从这一次失败推算字符上限。原生 4096 对照支持继续按客户端选择映射真实预算，不支持对所有工具请求注入统一的 4096 软限制。

已移除常规工具请求的整段系统分块提示和软预算注入，不替换成另一组数字；调用方系统指令保持原值。保留原生 `max_tokens` 映射、完整 JSON/帧/结束标记/截断处理。真实截断后的可配置缩块恢复仍保留，恢复中的 20/10 行和 4000/2000 字符是本地重试策略，不是上游上限，也不会预先附加到普通请求。

离线回归覆盖多个客户端预算、无/空/非空 system、各种工具名称，以及 `/v1`、`/cc/v1` 的 JSON、实时 SSE、缓冲 SSE，核对最终上游历史没有新增这些限制且调用方指令保持完整。

[脱敏原始记录](../testdata/kiro_tool_output_budget_live_2026-11-15.json)保存提示、旧系统策略、实际请求体哈希、request ID、metadata、原始工具参数、内容长度与逐字比较结果。[旧提示快照](../testdata/kiro_tool_output_policy_before_2026-11-15.txt)只供复现实验，生产代码不再引用。每个用例只有一次样本，固定先无提示再有提示；结果不能外推到所有模型、端点或其他日期。

复测（默认 8 次请求，会消耗额度；单请求 90 秒、响应最大 1 MiB，无重试）：

```sh
KIRO_LIVE_CREDENTIALS=/absolute/path/credentials.json \
KIRO_LIVE_CONFIG=/absolute/path/config.json \
KIRO_LIVE_ACCOUNT_ID=1 \
KIRO_CONTROLS_PROBE_OUTPUT=/absolute/path/output-budget-probe \
cargo test live_raw_tool_output_budget_probe --no-default-features -- --ignored --nocapture
```

可通过 `KIRO_CONTROLS_PROBE_CASES` 选择记录中的 `case` 标签。测试通过表示成功记录结果，包含 `MAX_TOKENS` 的未完成输出，不代表所有任务完成。
