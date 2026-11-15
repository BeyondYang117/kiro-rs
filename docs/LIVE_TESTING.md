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

已移除常规工具请求的整段系统分块提示和软预算注入，不替换成另一组数字；调用方系统指令保持原值。保留原生 `max_tokens` 映射、完整 JSON/帧/结束标记/截断处理。当时保留的截断恢复提示已在下一轮实测后移除 20/10 行、4000/2000 字符及强制骨架等要求，见下节；恢复尝试次数和触发边界仍保持原配置。

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

## 截断恢复提示实测

2026-11-15，继续使用同一导入账号、CLI / us-east-1 / `claude-haiku-5.5` 做 **6 次**真实上游请求。先发送要求复制 550 行、30250 字符的工具任务，实际原生参数为 `max_tokens:4096, thinking:{type:"disabled"}`。上游确实返回 `MAX_TOKENS` 和未完成工具 JSON，才允许进入恢复对照；没有伪造一个“之前失败”的普通请求。所有分支复用该失败请求的实际 body、会话 ID、模型参数、工具定义与历史，只允许当前用户消息内容变化。没有执行任何工具，没有把失败片段写入历史或伪造工具结果。

20/10 行与 4000/2000 字符均来自本地提交 `5b15337`（2026-11-04），相关提交没有附这些具体数值的官方依据或可复查真实边界实验。

| 分支 | 上游结果 | 完成原任务 |
|------|----------|------------|
| 初始请求 | `MAX_TOKENS`，工具 JSON 未完成 | 否 |
| 原请求逐字重发，请求体哈希相同 | 再次 `MAX_TOKENS`，工具 JSON 未完成 | 否 |
| 旧首次恢复：20 行/4000 字符、一个小调用、先写骨架 | 完整 JSON，但仅前 10 行、550 字符 | 否 |
| 旧第二档累积提示：再追加 10 行/2000 字符 | 完整 JSON，但仅前 5 行、275 字符 | 否 |
| 新首次恢复：说明截断、保留任务范围、完整再生成，不加大小数字 | 返回无法在当前预算可靠完成的说明，无工具调用 | 否，明确说明限制 |
| 新第二档累积提示 | 同样返回说明，无工具调用 | 否，明确说明限制 |

第二档是针对同一真实失败请求的**累积提示分支对照**，不是所有首次结果都触发了第二次生产恢复。尤其新首次结果是正常文本结束，生产逻辑不会据此继续自动重试；新第二档只验证提示累积后的行为。每个分支仅一个样本，所有 HTTP 都为 200，不能把 HTTP 成功或 JSON 完整算成任务恢复成功。上游没有报告精确输出 token 数。

结果说明：没有证据支持这些数值是恢复最优值或上游限制。旧提示确实能诱导生成很小的合法 JSON，但改变了调用方要求的完整任务；本次不能据此声称它成功恢复了 550 行任务。新提示也没有提高当前低预算下的任务完成率，因此不宣传“新恢复一定成功”。上节独立实测中，同一任务在原生预算 16384、无大小提示时完整返回；本轮恢复保持调用方预算，未擅自提高它。

生产改动：恢复提示去掉固定行数/字符数、强制单个调用、先写骨架和禁止改用工具等要求；只说明失败片段未交给客户端、未执行，要求完整重新生成，保留调用方任务/内容/范围，无法完成时说明限制。不引入另一组数字、不提高原生预算、不新增参数 schema 校验。恢复次数配置、已释放内容不得重放、声明工具检查、帧/JSON/截断边界继续保留。

离线回归覆盖首次与第二次恢复，以及 `/v1`、`/cc/v1` 的 JSON/实时 SSE/缓冲 SSE；核对重试仅追加当前用户内容，其他请求字段不变，并确认 80 行、超过 4000 字符的完整恢复调用不会被本地拦截。

[脱敏真实记录](../testdata/kiro_tool_recovery_live_2026-11-15.json)保存原始提示、实际发送的当前用户内容、请求体哈希、request ID、停止原因、工具原始参数及逐字匹配结果。[旧恢复提示快照](../testdata/kiro_tool_recovery_policy_before_2026-11-15.json)仅供实验复现。结论仅适用于本次账号、模型、端点、区域与日期；没有建立普遍恢复成功率。

复测（默认 6 次请求，会消耗额度，单请求 90 秒/1 MiB，不执行工具）：

```sh
KIRO_LIVE_CREDENTIALS=/absolute/path/credentials.json \
KIRO_LIVE_CONFIG=/absolute/path/config.json \
KIRO_LIVE_ACCOUNT_ID=1 \
KIRO_CONTROLS_PROBE_OUTPUT=/absolute/path/recovery-probe \
cargo test live_raw_tool_recovery_probe --no-default-features -- --ignored --nocapture
```

该测试要求基线确实发生工具截断，否则停止，不把普通请求宣称为真实恢复实测。测试通过表示实验被记录，包含任务未完成的结果。

## 请求改写继续审查

2026-11-15，在 CLI / us-east-1 / `claude-haiku-5.5`，用明确导入的同一账号发送 **24 次**原始请求（13 次 200、11 次 400）。工具、历史与模型参数均在端点/账号转换后注入，再核对实际请求体和 SHA-256，避免被待审查逻辑提前改写。每个用例一次，不自动重试，不执行工具。

| 候选逻辑 | 真实上游结果 | 决定 |
|----------|--------------|------|
| 工具名大小写不敏感合并 | `Probe` 与 `probe` 同时被接受，分别返回精确名称和不同标记参数 | 改为精确名称，保留两个定义及调用；仅完全相同名称的相同定义去重 |
| 工具名上限 63 | 63、64 被接受，65、128 被 400 拒绝；两个不同的 64 字符名字返回精确名称 | 合法名称上限改为 64，更长仍可逆缩名 |
| 非字母数字/下划线/连字符名称清理 | 点号、冒号、中文均 400 `Invalid tool use format` | 保留可逆别名适配 |
| 相同工具名重复定义 | 两个完全相同定义也被 400 拒绝 | 保留精确重复去重，冲突精确同名定义仍报错 |
| 自动补历史工具定义 | 无当前定义时历史结果仍能读取，结果放在 currentMessage 也成功；有定义时可再次调用，无定义时未返回调用 | 移除自动补定义，保留历史及结果，避免擅自扩大当前可用工具，修复 `tool_choice:none` 重新启用历史工具的问题 |
| Haiku 5.5 原生 thinking 改写及 display 过滤 | `enabled` 不在上游枚举；`adaptive+budget_tokens` 明确拒绝额外字段；`display:full` 不在枚举 | 保留已验证的 native 类型/预算整形和 display 兼容省略，不因这些 400 外推到全部模型 |
| 旧协议预算压到 24576 | 24576、30000 及 `NOT_A_NUMBER` 标签均返回 `LEGACY_OK` | 移除本地 clamp，保留显式值；接受不证明标签预算严格生效，未宣称新原生能力 |
| 静默删除末尾 assistant prefill | currentMessage 为 assistant 被明确拒绝；assistant 历史前缀加空 current user 也被拒绝 | 没有有效映射；非空内容明确 400，空占位仍兼容，不再静默丢失调用方内容 |

名称测试还有反例：全为 `n` 的 64 字符工具声明被接受，但模型回了 100 个 `n` 的工具名；随后补测的两个不同 64 字符名称均精确返回。没有把 HTTP 200 当成名字一定正确的保证，也没有新增返回参数 schema 校验。64 字符边界只在本次模型/账号上验证。

离线回归覆盖最终 CLI 原生/旧协议请求、JSON、实时及缓冲 SSE、精确大小写名称、64 字符名、`tool_choice:none` 携带历史工具、非空 prefill 明确报错及空占位兼容。历史名称映射继续保留，单纯历史引用不生成当前可调用工具。

[脱敏原始记录](../testdata/kiro_request_rewrite_audit_live_2026-11-15.json)包含全部注入定义、历史、提示、上游错误、request ID、请求体哈希与原始工具调用。未新增 IDE、跨提供商 reasoning 历史回放、全部模型枚举或压缩边界的真实验证；这些逻辑没有因本轮猜测而改动。旧协议 thinking 标签仅在调用方显式请求 thinking/effort 时沿用，未证明严格生效，也未擅自改成默认原生模式。系统指令转 user/assistant 历史配对属于当前 Kiro 协议映射，本轮未证明存在更优的原生 system 入口。

复测（默认 24 次请求，会消耗额度，单请求 90 秒/1 MiB，无工具执行）：

```sh
KIRO_LIVE_CREDENTIALS=/absolute/path/credentials.json \
KIRO_LIVE_CONFIG=/absolute/path/config.json \
KIRO_LIVE_ACCOUNT_ID=1 \
KIRO_CONTROLS_PROBE_OUTPUT=/absolute/path/request-rewrite-audit \
cargo test live_raw_request_rewrite_probe --no-default-features -- --ignored --nocapture
```

可通过 `KIRO_CONTROLS_PROBE_CASES` 选择记录中的 `case`。测试通过表示记录完成，不代表所有候选被支持或所有返回都符合调用方意图。


## thinking 历史与压缩实测

2026-11-15，使用同一导入账号，在 CLI / us-east-1 对 Haiku 5.5 及 GPT 历史对照执行 **24 次真实上游调用**（21 次 HTTP 200、3 次 HTTP 400）。[脱敏记录](../testdata/kiro_thinking_compaction_live_2026-11-15.json)保留全部结果、候选 patch、请求体 SHA-256、request ID、上游停止事件和错误。字段在端点/模型转换后加入，并断言实际发送的 HTTP body 与候选相同；每例单次尝试，不执行工具。真实 reasoning 签名只留在内存用于原样重放，记录仅保存签名长度和哈希。

| 对照 | 真实结果 | 调整 |
|---|---|---|
| 无 thinking 标签、enabled 30000、adaptive/high、非法标签及原生 adaptive | 简单算术均返回 391，均无 reasoning 事件；非法标签也 200 | 标签没有验证出原生控制语义，移除旧协议提示注入并记录显式设置未映射警告，不默认开启 nativeThinking |
| Haiku 无签名 / 伪造签名 reasoning 历史 | 均 400 `THINKING_SIGNATURE_INVALID` | 不制造签名或改写推理文本 |
| 从真实响应捕获的完整 reasoning 文本与签名，原样重放至旧协议 / native adaptive / GPT | 均 200，回答 391 | 移除按旧协议或非 Claude 类别删除 reasoning 的逻辑 |
| 合成 redacted / GPT 合成签名历史 | 200，但隐藏标记没有被复述 | 不据此声称认证成功或模型使用了该历史；未取得真实 redacted 重放证据 |
| 短对话摘要，disabled / native adaptive | 均 200，正确摘要标记 | 取消 50000 最小调度阈值，允许正整数；摘要不再强制 disabled 或删除 effort / 模型别名 |
| 约 31k 字符的正常多段参考指令，disabled / adaptive | 均 200，正确保留 LONG_SUMMARY_OK | 取消自定义摘要指令的 16384 字符限制 |
| 约 20k 重复参考文本 | HTTP 200，但模型拒绝照做 | 仅证明请求被接受，不当成指令遵从证据 |
| 64×64 RGB PNG 原图 / 无图占位对照 | 原图正确描述红色方块，无图回答 IMAGE_UNAVAILABLE | 摘要传真实图像附件，保留正文位置标记，含嵌套工具结果中的图片 |
| 旧协议未发送原生 max_tokens，要求完整复制 150 行摘要 | 200、END_TURN，9325 字符，本地估算 2331，超过测试输入 max_tokens=1024 | 不再把本地估算超限改成 max_tokens 停止原因 |

初次图片使用 1×1 灰度 PNG，收到 `IMAGE_FORMAT_UNSUPPORTED`；随后换成有效 RGB PNG 重测成功。保留该失败记录，不把格式失败解释成上游不支持图片。输出 token 数未由上游精确报告，本地估算 2331 不是实测计费 token 数。

重建并重启服务后，另经本地 `/v1/messages` 完整生产链路发送阈值 1、22978 字符自定义指令、adaptive/low 设置及标准 PNG。返回 HTTP 200、单次生成、`stop_reason=compaction`，摘要包含 PRODUCTION_COMPACTION_OK 并正确识别红色图片，见[生产链路记录](../testdata/kiro_thinking_compaction_production_live_2026-11-15.json)。默认旧协议设置产生未映射警告，没有注入 thinking 标签。冷启动的第一次补验因配置的总尝试预算为 1，在发起生成前耗尽预算而返回 503（生成 attempts=0）；随后独立请求成功，未修改或提高预算。离线验证 `cargo test --all-targets --no-default-features`：620 项通过、8 项付费联调默认跳过。

触发阈值是代理调度选项，不是透传的 Kiro 原生字段；短对话摘要成功说明不需要 50000 的上游边界，生产链路的低阈值由离线测试覆盖。以上结果仅适用于所测账号/模型/端点/区域，不代表所有标签永远无效、所有签名可跨模型使用，或所有图片格式均支持。前节旧版标签/历史清理的保留说明已被本轮后续证据与修改取代。

压缩本身仍按调用方请求生成摘要：工具定义作为参考文本而不开放执行；历史被放入 transcript；默认摘要指令只在没有自定义 instructions 时使用。传统 Claude Code 总结系统句触发的兼容转换仍保留，以容纳其末尾 assistant 内容，未把该行为扩展到其他普通请求。以上属于代理压缩协议行为，不声明为 Kiro 的硬性限制。

复测会消耗额度：

```sh
KIRO_LIVE_CREDENTIALS=/absolute/path/credentials.json \
KIRO_LIVE_CONFIG=/absolute/path/config.json \
KIRO_LIVE_ACCOUNT_ID=1 \
KIRO_CONTROLS_PROBE_OUTPUT=/absolute/path/probe-output \
cargo test live_raw_thinking_compaction_probe -- --ignored --nocapture
```

可用 `KIRO_CONTROLS_PROBE_CASES` 选择逗号分隔的 case 名；重放 `history-*-captured` 时必须同时选择先前能产生真实签名的用例（本次为 `history-none`），若上游未返回真实签名则明确跳过，不能宣称重放验证通过。默认离线测试不访问真实上游。


## 多段 reasoning 与传统摘要改写实测

2026-11-15，继续使用同一导入账号、CLI / us-east-1 / Haiku 5.5。共保存 **42 次真实上游调用**（28 次 HTTP 200、14 次 HTTP 400），包括各批次独立获取真实签名及修改后转换器补验。见[脱敏记录](../testdata/kiro_multi_reasoning_legacy_live_2026-11-15.json)。候选字段在端点转换后设置，实际 HTTP body 与最终候选相等并记录 SHA-256；签名只在内存中保留用于重放，记录仅保存长度和哈希，不执行工具。

| 候选 / 对照 | 上游结果 | 结论 |
|---|---|---|
| 两个独立捕获的真实签名分别放入单块 reasoningContent | 均 200 | 单块对照有效；捕获响应常为 MAX_TOKENS，但真实签名仍可被接受 |
| reasoningContent 数组、reasoningText 数组、assistant content 块数组 | 均 400 REQUEST_BODY_INVALID | 所测数组入口不能直接承载多段；不据此推断所有表示均不支持 |
| reasoningContents 复数字段，真实数组 / 非法字符串对照 | 均 200 | 未证明该字段生效，不能拿 HTTP 200 当成内容被使用的依据 |
| 连续两个 assistant 均带真实签名和非空正文 | 200 MULTI_REPLAY_OK | 可用多个原生 assistant 条目保留阶段，不必丢掉第一段 |
| 第一段签名改为非法值，其他内容相同 | 400 THINKING_SIGNATURE_INVALID，定位 messages.1.content.0 | 第一段确实参与校验，不是被忽略 |
| 第一段有正文、第二段为空 / 第一段为空、第二段有正文 / 第一段只有空格 | 分别 200 / 400 / 400 | 不能简单概括为“连续 assistant 不支持”或“空正文总是支持” |
| 第一段无正文但带工具；以及两段分别带工具并由当前 user 返回两个结果 | 均 200 | 工具调用和结果可跨 reasoning 阶段保存；没有执行实际工具 |
| 三段 signed 或 signed/redacted 混合 | 有 200 也有 400 | 不制定本地最大段数；synthetic redacted 接受不代表真实加密块重放已经验证 |
| 两个真实 user/assistant 历史轮次 | 200 | 普通跨轮 reasoning 保持可用 |
| 修改后的生产转换器：同一 content 多块 / 连续 assistant 两种输入，含两个工具调用和结果 | 均 200 MULTI_REPLAY_OK，实际 body 断言保留两段原文与签名及两个结果 | 取消“只保留最后一段”；不拼接签名、不新增虚构 user 或提示词 |
| 传统摘要系统句，保留原历史与最终 user 指令 | 200，一句话包含 LEGACY_ORIGINAL_OK、ALPHA_71、BETA_93，无 XML | 普通转换已能满足请求，无需自动摘要重写 |
| 同一请求经旧自动重写逻辑处理 | 200，但返回多段 `<summary>` 摘要，违背“一句话、无 XML”要求 | 移除根据系统句自动改写、默认指令注入、工具撤除；旧请求已保存为[对照 fixture](../testdata/kiro_legacy_summary_rewrite_before_2026-11-15.json) |
| 传统摘要末尾 assistant 作为 currentMessage / 历史后补空 user | 均 400 | 不伪造新请求以绕过；与普通 prefill 一样明确报错，调用方需以 user 结束或显式使用代理 compaction |

转换器将同一 assistant 回合的 reasoning 阶段按顺序放入连续 assistant 历史，保留各段正文和工具所在位置；工具配对及跨轮 ID 重映射以整个 assistant 回合为单位，防止拆段后丢掉早期工具结果。部分纯推理或空正文组合仍可能被 Kiro 拒绝，原样透出上游错误，不本地增加段数校验或静默退回“只留最后一段”。不含多个有效 reasoning 的普通 assistant 仍沿用原有合并规则。

传统摘要系统句不再触发特殊行为，适用于两个消息接口、JSON、实时 SSE、缓冲 SSE，以及未到阈值的 context_management 请求；明确的 compaction/context_management 压缩功能保持按调用方配置执行。此前文档“只保留最后块”和“保留传统系统句触发转换”的说明被本轮证据取代。

修改后已重建并重启 8990 服务；通过真实 `/v1/messages` 再发传统摘要系统句、原始历史、一句话/无 XML 指令及工具定义，上游返回 HTTP 200 / end_turn，保留三个标记且无 summary 包装，见[生产链路记录](../testdata/kiro_legacy_summary_production_live_2026-11-15.json)。该调用独立于上述 42 次原始探针。`cargo test --all-targets --no-default-features` 最终 621 项通过、9 项付费联调默认跳过；构建、格式及差异检查通过。

第二批标签 multi-reasoning-contents-invalid 误用了数组候选，未拿它当无效值证据；第三批 multi-plural-invalid-control 才是非法字符串对照。第一批在工具上下文的本地探针断言处停止，已发送的 8 个结果均保留，后续分批完成。另一次简单算术捕获没有返回签名，探针停止且未记录完整结果；没有据此声称重放成功。

复测会消耗额度：

```sh
KIRO_LIVE_CREDENTIALS=/absolute/path/credentials.json \
KIRO_LIVE_CONFIG=/absolute/path/config.json \
KIRO_LIVE_ACCOUNT_ID=1 \
KIRO_CONTROLS_PROBE_OUTPUT=/absolute/path/probe-output \
cargo test live_raw_multi_reasoning_legacy_probe -- --ignored --nocapture
```

可用 KIRO_CONTROLS_PROBE_CASES 选择逗号分隔 case；多段重放需要同时选择 multi-capture-a 和 multi-capture-b，未取得两个真实签名即停止。结果仅适用于所测账号/端点/区域/模型，不声明其他模型的全部历史表示均兼容。


## system 配对与工具历史清理实测

2026-11-15，同一导入账号、CLI / us-east-1 / Haiku 5.5，共 **63 次真实请求**：45 次 HTTP 200、18 次 HTTP 400，包含修改后生产转换器的 5 次补验。见[脱敏记录](../testdata/kiro_system_orphan_history_live_2026-11-15.json)。每例在端点/模型转换后设置候选，断言实际 HTTP body 与最终候选一致并计算 SHA-256；候选及实际 conversationState、上游 request ID、错误、停止原因和生成文本均保留。工具仅为合成历史，未执行任何工具，也未自动重试。

| 对照 | 上游结果 | 修改依据 |
|---|---|---|
| system 原文转首个 user + “I will follow these instructions.” assistant | 200，部分冲突提示返回 system 标记；另一普通指令对照未按标记回答 | 配对会影响模型行为，不能把伪造确认视为无害协议占位 |
| 单独历史 user、空/空格 assistant、当前 user 前缀 | 均可 200，标记读取与冲突结果存在差异 | 不能仅据 HTTP 200 判断等价；也不能宣布空 assistant 是有效替代 |
| root / conversationState / user context 的 system 字段，及非法数值对照 | 均 200，但未返回候选标记；非法值同样接受 | 未证实字段有效，不采用“原样加字段就生效”的猜测 |
| root systemPrompt 字符串、数组、text/content 对象及非法值 | 均 400；state / user / context 同名候选则 200，但未执行标记 | 所测候选未找到有效原生 system 入口；不外推所有可能字段或模型都不支持 |
| systemMessage 历史角色 | 400 | 不新增假定原生 role |
| 仅把调用方 system 原文放在首个真实 user 之前，不加角色说明或确认消息 | 无历史、普通历史、工具结果历史均 200；修改后转换器也 200，普通指令返回 SYSTEM_CONVERTED_67 | 采用无虚构 assistant 的文本映射；相对后续 user 的严格优先级不保证，记录警告 |
| 历史首个配对 / 后续配对 / 未配对 user 的标记读取与冲突 | 都 200，但 UNKNOWN、标记及优先级行为不同 | 不补 OK 制造历史；不能以个别 UNKNOWN 断言上游一律丢弃所有未配对 user |
| 正常 tool_use + 当前 tool_result | 200，返回调用参数及结果标记 | 配对控制成功 |
| 无结果 tool_use（当前前一轮或更早）、孤立当前/历史 tool_result、跨轮结果、先结果后调用、重复当前/历史结果 | 均 400 TOOL_USE_RESULT_MISMATCH，上游明确要求相邻调用和单个结果 | 移除历史/当前结果过滤、孤立调用删除；保留原块并透出上游错误，不把无效请求悄悄改成另一段历史 |
| 修改后转换器：孤立调用、孤立结果、重复结果 | 均保留到实际 body，收到 400 TOOL_USE_RESULT_MISMATCH | 不是只在原始探针中绕开清理，实际转换器也不再丢块 |

system 映射只将原文与首个真实 user 文本用换行连接，不插入“低优先级 user”之类额外提示。原生 system 的优先级尚未得到验证，正文保留不代表角色语义完全等价；各单次冲突/回忆输出亦不当成严格执行证明。不同轮次/措辞的结果确实有差异，记录包含这些反例，不选择性报告。

自动补 OK 的兜底分支在正常 convert_request 路径不可达：末尾连续 user 已整体变成 currentMessage，历史切片必以 assistant 结束。仍移除该兜底，并通过直接历史构建回归验证，未配对 user 不会增加虚构 assistant。正常末尾连续 user 的原文、图片、工具结果仍合并为当前输入。

已有工具 ID 复用的确定性重映射和同回合重复调用处理不属于本轮新增行为，本轮不根据工具配对 400 扩展新的本地清理规则。移除清理后，保留调用方孤立结果正文、重复结果和未完成调用；上游拒绝只尝试一次，JSON / SSE 两种消息接口均覆盖错误和实际 body 回归。

重建并重启服务后，经真实 `/v1/messages` 的 system 请求返回 SYSTEM_PRODUCTION_37；孤立 tool_result 经 `/v1/messages`、孤立 tool_use 经 `/cc/v1/messages` 均返回 400 TOOL_USE_RESULT_MISMATCH，日志各记录 1 次上游尝试。见[生产接口记录](../testdata/kiro_system_orphan_history_production_live_2026-11-15.json)。其中第一次孤立结果补验遇到本地总尝试预算 1 耗尽，生成 attempts=0 / HTTP 503；该记录保留，随后独立请求取得真实 400，未修改预算。这 4 次接口请求独立于上述 63 次原始探针。

最终 `cargo test --all-targets --no-default-features`：614 项通过、10 项付费联调默认跳过；移除了专门验证旧静默清理实现的测试，增加内容保留、无虚构 OK、JSON / SSE 错误透传回归。构建、格式及差异检查通过，服务保持运行。

复测会消耗额度：

```sh
KIRO_LIVE_CREDENTIALS=/absolute/path/credentials.json \
KIRO_LIVE_CONFIG=/absolute/path/config.json \
KIRO_LIVE_ACCOUNT_ID=1 \
KIRO_CONTROLS_PROBE_OUTPUT=/absolute/path/probe-output \
cargo test live_raw_system_orphan_history_probe -- --ignored --nocapture
```

KIRO_CONTROLS_PROBE_CASES 可筛选逗号分隔 case 名。结果仅适用于所测账号/模型/端点/区域，不能视为全部 Kiro 协议的字段支持清单。前节中依赖原 system 配对生成的历史测试记录保留为当时行为，不代表当前仍注入该配对。
