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
