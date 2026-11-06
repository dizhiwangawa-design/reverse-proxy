# WorkBuddy Proxy

把本机 WorkBuddy 桌面端账号变成一个 **OpenAI / Anthropic 兼容** 的本地 API 网关，并提供中文管理台。适合给 Claude Code、Cline、Continue、OpenAI SDK、LangChain 等工具使用。

- 多账号池：轮询 / 最少使用 / 加权随机三种调度策略
- 自动续期：access token 快过期时自动调用 WorkBuddy 刷新接口，失败自动冷却并换号重试
- 双协议：`/v1/chat/completions`（OpenAI）与 `/v1/messages`（Anthropic），包含 SSE 流式转发、工具调用和图片输入
- 本机会话同步：可直接导入 WorkBuddy 桌面端的登录态，并跟随桌面端刷新令牌
- 管理台：运行概览、账号池、模型目录、接入密钥、在线调试、调用日志、设置
- 数据本地化：状态保存在 `data/state.json`（权限 `600`），默认只监听 `127.0.0.1`

## 快速开始

### 依赖

- Rust 1.80+（项目使用 edition 2021）
- Node.js 18+（仅构建前端时需要）

### 构建

```bash
make build
# 等价于:
#   cd web && npm ci && npm run build
#   cargo build --release
```

生成的可执行文件：`target/release/workbuddy-proxy`。

> `web/dist/` 会被编译进二进制（rust-embed），所以修改前端后需要重新执行 `cargo build --release`。`build.rs` 会在 `web/dist` 变化时触发重新编译。

### 启动

```bash
./target/release/workbuddy-proxy
```

启动后会打印 Web UI、OpenAI/Anthropic 地址和管理令牌，例如：

```text
  Web UI       http://127.0.0.1:8317/
  OpenAI API   http://127.0.0.1:8317/v1
  Anthropic    http://127.0.0.1:8317/v1/messages
  Admin token  wbadmin_...
  API key      sk-wb-...
```

### 首次使用

1. 打开 <http://127.0.0.1:8317/>，用终端打印的管理令牌登录（也可读取 `data/admin_token.txt`）。
2. 在「运行概览」或「账号池」点击 **导入本机账号**，程序会读取：
   `~/.local/share/CodeBuddyExtension/Data/Public/auth/workbuddy-desktop.info`
   如果桌面端不在默认路径，可在「设置」里修改「本机认证文件」。
3. 也可以手动添加账号：粘贴 accessToken（可选 refreshToken），UID 留空会自动从 JWT 解析。
4. 在「接入密钥」新建客户端密钥，供其他 Agent 使用。

## 配置

配置文件为项目目录下的 `config.toml`（可用 `WB_CONFIG` 指定其他路径），完整示例见 [config.example.toml](config.example.toml)。

- `config.toml` / 环境变量中**显式写出**的轮询配置会覆盖管理台保存的设置；
- 没有写出的字段不会覆盖 `data/state.json` 里由管理台保存的值；
- 上游地址、监听地址、管理令牌只能通过配置或环境变量修改。

优先级（从高到低）：命令行环境变量 > `config.toml` > 管理台保存值 > 内置默认值。

### 环境变量

| 变量 | 说明 | 默认值 |
| --- | --- | --- |
| `WB_DATA_DIR` | 数据目录（`state.json`、`admin_token.txt`） | `data` |
| `WB_CONFIG` | 配置文件路径 | `config.toml` |
| `WB_PROXY_BIND` | 监听地址 | `127.0.0.1:8317` |
| `WB_ADMIN_TOKEN` | 管理令牌 | 读取/生成 `data/admin_token.txt` |
| `WB_UPSTREAM_ENDPOINT` | WorkBuddy 上游地址 | `https://copilot.tencent.com` |
| `WB_REQUIRE_API_KEY` | 是否要求客户端密钥（`1`/`true`/`yes`） | `true` |
| `WB_LOCAL_AUTH_PATH` | 本机 WorkBuddy 登录态文件 | 见上方默认路径 |
| `RUST_LOG` | 日志级别，例如 `workbuddy_proxy=debug,tower_http=info` | `workbuddy_proxy=info,tower_http=warn` |

### 开机自启（可选，systemd user 服务）

```bash
mkdir -p ~/.config/systemd/user
cp deploy/workbuddy-proxy.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now workbuddy-proxy
systemctl --user status workbuddy-proxy
```

示例假定项目位于 `~/reverse-proxy`，如不同请修改 unit 里的 `WorkingDirectory` 与 `ExecStart`。

## 客户端接入

### OpenAI 兼容

Base URL：`http://127.0.0.1:8317/v1`

```bash
curl http://127.0.0.1:8317/v1/chat/completions \
  -H "Authorization: Bearer sk-wb-..." \
  -H "Content-Type: application/json" \
  -d '{
    "model": "deepseek-v4.1-flash",
    "messages": [{"role": "user", "content": "你好"}],
    "stream": true
  }'
```

`model` 可使用管理台「模型目录」里的任意上游模型 id。请求未提供 `model` 时使用设置里的「默认模型」（默认 `auto`）。

Python OpenAI SDK：

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8317/v1",
    api_key="sk-wb-...",
)
print(client.chat.completions.create(
    model="deepseek-v4.1-flash",
    messages=[{"role": "user", "content": "你好"}],
).choices[0].message.content)
```

### Anthropic / Claude Code

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:8317"
export ANTHROPIC_AUTH_TOKEN="sk-wb-..."
export ANTHROPIC_MODEL="deepseek-v4.1-flash"
claude
```

- 同时支持 `Authorization: Bearer <key>` 和 `x-api-key: <key>`；
- 支持 `/v1/messages/count_tokens`（为保证 Claude Code 的上下文统计正常工作，使用本地保守估算）；
- 支持流式、system、图片、`tool_use` / `tool_result` 转换。

管理令牌本身也可作为特权 API key 使用（便于调试台直接调用），但建议给每个客户端单独创建密钥。

## 账号轮询与令牌刷新

- **round_robin**：按顺序轮流使用可用账号；
- **least_recent**：优先使用最久未使用的账号；
- **weighted**：按账号权重随机选择。

单个请求最多尝试「单请求最多尝试账号数」个账号。上游返回 429、5xx 或网络错误时，该账号会进入冷却；401/403 会先尝试刷新令牌再重试。冷却时长可在「设置」调整。

后台每分钟检查一次令牌有效期，并在临期时刷新；同时按设置同步本机 WorkBuddy 登录态文件。代理刷新令牌后不会回写桌面端的登录文件，因此建议优先把桌面端和代理的令牌都保持可用；如果桌面端重新登录，代理会在下一次同步时采用更新的令牌。

## 管理 API

除 `/health` 与 `/admin/api/login` 外，`/admin/api/*` 需要管理令牌：

```bash
curl -H "Authorization: Bearer $WB_ADMIN_TOKEN" http://127.0.0.1:8317/admin/api/overview
# 或 -H "x-admin-token: $WB_ADMIN_TOKEN"
```

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| `GET` | `/health` | 健康检查 |
| `GET` | `/admin/api/overview` | 汇总、账号健康度、最近请求 |
| `GET/POST` | `/admin/api/accounts` | 列出 / 添加账号 |
| `PATCH/DELETE` | `/admin/api/accounts/{id}` | 修改 / 删除账号（支持替换令牌） |
| `POST` | `/admin/api/accounts/{id}/refresh` | 强制刷新令牌 |
| `POST` | `/admin/api/accounts/{id}/test` | 连通性测试 |
| `POST` | `/admin/api/accounts/import-local` | 导入本机登录态 |
| `GET` | `/admin/api/models?refresh=1` | 模型目录（可强制刷新） |
| `GET/POST` | `/admin/api/keys` | 列出 / 创建客户端密钥 |
| `DELETE` | `/admin/api/keys/{id}` | 删除密钥 |
| `GET/PUT` | `/admin/api/settings` | 读取 / 保存设置 |
| `GET` | `/admin/api/logs` | 最近 100 条请求与总计 |
| `GET` | `/admin/api/info` | 版本、监听地址、上游地址 |

## 数据与安全

- `data/state.json` 保存账号令牌、API 密钥、设置、统计与最近请求日志，文件权限为 `600`；
- 令牌以明文保存在本地，请勿提交到版本库（`.gitignore` 已忽略 `data/`）；
- 默认只监听 `127.0.0.1`。如果改成 `0.0.0.0` 暴露到局域网/公网，请务必启用客户端密钥、配置反向代理 TLS，并限制来源；
- 默认 CORS 为 `*`，可通过 `config.toml` 的 `cors_origins` 收紧；
- 管理令牌等同于最高权限，泄露后请删除 `data/admin_token.txt`（或修改 `WB_ADMIN_TOKEN`）并重启。

## 开发

```bash
make check          # fmt + clippy + tsc + test
make smoke-local    # 在允许监听 loopback 端口的环境中验证本地生命周期
make web            # 构建管理台
./scripts/smoke.sh  # 对运行中的实例做端到端冒烟测试
```

`smoke-local` 使用空的临时数据目录和 `127.0.0.1:18317`（`WB_SMOKE_PORT` 可覆盖），不需要 WorkBuddy 账号或上游额度。

目录结构：

```text
src/
  main.rs        启动、路由、后台任务
  config.rs      config.toml / 环境变量
  state.rs       账号池、轮询、刷新、模型缓存、共享状态
  store.rs       数据持久化、JWT 解析
  upstream.rs    WorkBuddy 上游客户端
  sse.rs         SSE 解析
  tokens.rs      count_tokens 估算
  api/
    mod.rs       Axum 路由、鉴权、静态资源
    openai.rs    OpenAI 兼容接口
    anthropic.rs Anthropic 兼容接口
    admin.rs     管理 API
web/              React + Vite 管理台
data/             运行数据（不提交到 git）
```

## 常见问题

**页面提示 “UI is not built”**
前端没有构建，或构建后没有重新编译 Rust。执行 `make build`。

**401 invalid API key**
客户端没有带密钥，或密钥被删除；检查「接入密钥」页面与请求头。`/v1` 同时接受 `Authorization: Bearer` 和 `x-api-key`。

**503 no_available_account**
账号池为空，或所有账号都在冷却/失效。到「账号池」查看状态并测试连通性，必要时刷新或替换令牌。

**上游 model not found**
`model` 必须是「模型目录」中的上游模型 id（例如 `deepseek-v4.1-flash`），不能是 Claude/OpenAI 的模型名，除非该名称确实在上游目录中。

**端口被占用**
用 `WB_PROXY_BIND=127.0.0.1:8318 ./target/release/workbuddy-proxy` 临时换端口，或修改 `config.toml`。

## License

GPL-3.0-or-later（GNU General Public License v3.0 或更新版本），见 [LICENSE](LICENSE)。

本项目为自由软件：你可以依据自由软件基金会发布的 GNU 通用公共许可证（第 3 版或你选择的任何更新版本）条款重新分发和/或修改它。

本程序基于“有用”的目的分发，但不提供任何担保，甚至不提供适销性或特定用途适用性的默示担保。详见 GNU 通用公共许可证。

