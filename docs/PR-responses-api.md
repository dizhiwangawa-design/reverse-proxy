# PR: 新增 OpenAI Responses API 兼容层（Codex CLI / OpenAI Agents SDK 可直连）

| 项目 | 内容 |
|---|---|
| 分支 | `feat/responses-api` → `master`（基于 `fe9c6d5`） |
| 类型 | `feat`（新增能力，不改既有行为） |
| 影响面 | 新增 3 个路由；既有 `/v1/chat/completions`、`/v1/messages`、管理台不受影响 |
| 代码量 | 新增 1 个模块（1751 行，含 17 个单测）、文档 725 行、脚本 85 行；修改 4 个文件（约 30 行） |
| 关联文档 | [`docs/responses-api.md`](./responses-api.md)（实现手册：字段映射、事件序列、坑位清单、排障决策树） |
| 破坏性变更 | 无（`/v1/responses` 此前被兜底路由返回 `404 route not found`） |

---

## 1. 摘要

- 新增 `POST /v1/responses`，把 OpenAI Responses 协议翻译成上游的流式 chat completions，再把结果与 SSE 事件还原成 Responses 形态；`GET|DELETE /v1/responses/{id}` 明确返回 404（不持久化）。
- 打通 Codex CLI 的完整工具闭环：`function` 与 `custom`（`apply_patch`）两类工具双向映射，压缩成"模型只会讲、不会改"的问题从根上消除。
- 补齐编码代理需要但协议默认不保证的三件事：工具类型保真、回合收尾（`completed`/`failed` 必达）、reasoning item 的可信表达（不伪造 `encrypted_content`）。

## 2. 背景与目标

Codex CLI 0.160、OpenAI Agents SDK 等新客户端默认走 Responses 协议（`wire_api = "responses"`），而本项目上游（WorkBuddy copilot 网关）只实现流式 chat completions。接入方把 base URL 指到本代理后拿到：

```
HTTP 404 Not Found
{"error":{"type":"not_found","message":"route not found: /v1/responses"}}
```

目标（按验收顺序）：

1. `/v1/responses` 可用：文本、多模态输入、流式与非流式、工具调用、usage 统计；
2. 真实 Codex CLI 能在一个 turn 内连续完成"读代码 → 打补丁 → 跑测试 → 再修"，不需要人工输入"继续"；
3. 不改变既有接口行为，不引入上游不支持的语义（尤其是 reasoning 的加密存档）。

## 3. 方案概述

```
Codex CLI / Agents SDK                workbuddy-proxy                     上游 copilot 网关
─────────────────────                 ────────────────────               ───────────────────
POST /v1/responses                    responses::create                  POST /v1/chat/completions
  instructions / input[]   ──────────▶ build_messages()  → messages[]  ──▶ 仅流式 chat
  tools[] (function/custom) ────────▶ convert_tools()    → tools[]        无结构化 tool_calls
  max_output_tokens        ──────────▶ max_tokens                        tool call 藏在 content（DSML）
  text.format              ──────────▶ response_format
                                      chat_with_rotation()  ◀──────────── SSE chunk
  response.*  事件族        ◀───────── stream_body() / 聚合 → output[]
```

翻译层是纯函数式的请求改写 + 流式状态机，不新增上游依赖、不落库。

## 4. 变更清单

### 新增

| 文件 | 行数 | 说明 |
|---|---|---|
| `src/api/responses.rs` | 1751 | 路由处理器、请求翻译（`build_messages` / `push_input_item`）、工具桥接（`convert_tools` / `custom_tool_as_function` / `tool_item`）、响应与流式事件（`stream_body` / `output_items` / `base_response`）、usage 映射、`previous_response_id` 进程内历史、续跑约束（`CONTINUATION_NUDGE`）、17 个单测 |
| `docs/responses-api.md` | 725 | 实现手册：协议差异、映射表、事件序列不变量、工具桥接四类型矩阵、DSML 回收、回合语义、10 项缺口与落点、验证排障手册、开源实现对照、排障决策树 |
| `scripts/restart.sh` | 85 | 按端口停旧进程 → 起新二进制 → 等 `/health` → 跑 smoke；端口被外部命名空间占用时以 exit 2 拒绝动手，并透传 `WB_DATA_DIR` |

### 修改

| 文件 | 改动 |
|---|---|
| `src/api/mod.rs` | `pub mod responses;`；新增 `POST /v1/responses`、`GET|DELETE /v1/responses/{id}` |
| `src/api/admin.rs` | `/admin/api/info` 的端点清单补充 `"responses": ["/v1/responses"]` |
| `scripts/smoke.sh` | 新增 `responses (non-stream)`、`responses (stream)` 两项检查；统一 `no_proxy`，避免本地请求被 http_proxy 劫持 |
| `README.md` | 特性列表改为"多协议"；新增 Responses 配置示例与 `custom` 工具说明；目录树补 `responses.rs` / `docs/` |

## 5. 关键设计决策

**D1. 翻译放在代理层，而不是让客户端降级到 chat 协议。**
Codex 的 `wire_api` 可选 `chat`，但其工具循环、`apply_patch`、事件渲染都按 Responses 设计；让客户端降级等于放弃这些能力。代理层翻译是唯一能同时兼容多客户端（Codex / Agents SDK / 其他 `wire_api=responses` 实现）的位置。

**D2. `custom` 工具包装成"只收一个字符串的 function"，回包再还原成 `custom_tool_call`。**
chat 协议没有 freeform 概念。若直接丢弃非 `function` 工具，模型会从系统提示里"知道"要调 `apply_patch` 而凭空生成调用，代理按 `function_call` 回包，Codex 只认 `custom_tool_call` → 补丁被静默丢弃 → 表现为"只会讲、不会改"。`payload()` 同时接受 `{"input": "..."}` 包装与裸文本两种形态（模型经常不守包装）。

**D3. reasoning 只发真 item，不伪造 `encrypted_content`。**
候选方案有三种（真 item / 仅摘要事件 / 自定义可逆存档）。仅摘要事件会让 `reasoning_summary_text.delta` 的 `item_id` 指向一个不存在的 item，是 SDK 判错的典型；自定义存档超出上游能力且多轮往返必然断。因此：发完整 `reasoning` item（`summary` 有内容、`encrypted_content` **字段不存在**），`include: ["reasoning.encrypted_content"]` 不返回 400（Codex 默认带该字段，拒绝等于全盘失效），语义上就是"没有可交付的密文"。

**D4. 回合收尾交给协议保证，续跑提示做成可开关的兜底。**
客户端以"本轮响应里有没有工具调用"判定 turn 结束，模型只输出散文就会被当成完成。`build_messages` 在最后一条 user 消息后插入一段约束（未完成必须带工具调用 / 不要用"我正在…"顶替动作 / 验证通过才可输出总结 / 补丁失败要拆小 hunk 重试），并用 `WB_RESPONSES_NUDGE=0` 提供关闭开关——没有开关就无法做 A/B，也无法在个别模型上排除副作用。

### 备选方案（未采用）

| 方案 | 不采用的原因 |
|---|---|
| 只支持 `function` 工具，要求用户关掉 `apply_patch_freeform` | 依赖用户改客户端配置；一旦换客户端或版本升级就复发 |
| 用 `/v1/responses/compact` 之类扩展端点做压缩 | 上游无对应能力，属于给客户端画饼 |
| reasoning 直接塞进 message 的 content | 会把思考内容当正文渲染，污染输出与工具解析 |

## 6. API 行为

| 端点 | 方法 | 行为 |
|---|---|---|
| `/v1/responses` | POST | 支持 `instructions`、`input`（字符串或 item 数组）、`tools`、`tool_choice`、`text.format`、`max_output_tokens`、`temperature`、`top_p`、`parallel_tool_calls`、`stream`、`previous_response_id`；返回 `output[]` + `usage` 或 `response.*` 事件流 |
| `/v1/responses/{id}` | GET / DELETE | 恒 404，并在 message 中说明不持久化（比返回空壳更安全） |

响应 `status` 恒为 `completed`（`incomplete` 的映射列入后续工作，见 §10）。历史最多保留 64 条，仅进程内有效；Codex 每轮带全量 `input`，不依赖此机制。

## 7. 测试与验证

### 自动化

```bash
cargo test                              # 83 passed（其中 responses.rs 17 项）
BASE=http://127.0.0.1:8317 ./scripts/smoke.sh   # 10/10 PASS（含 responses 流式/非流式）
```

`responses.rs` 的单测覆盖：`instructions` 扁平化、`input[]` 各类 item 映射、并行工具调用合并、custom 工具包装与还原、裸文本载荷兜底、历史回放、usage 明细、reasoning item 位置与"无 `encrypted_content`"、续跑提示插入位置与关闭条件。

### 真实客户端（关键一步，代理单测证明不了"能干活"）

| 场景 | 配置 | 结果 |
|---|---|---|
| 建 3 个文件 + 跑测试 | 用户真实 `CODEX_HOME`（含 `model_catalog_json`） | 一个 turn 内完成，6 次命令 + 2 次补丁 |
| 六步重构（docstring / 类型标注 / 抽公共函数 / 4 个测试文件 / README / 全绿） | 同上 | 一次跑完，13 个测试全绿 |
| 增量改造（`Inventory` 类化 + `bonus.tier` + 补测试） | 同上 | 18 个测试全绿，`orders.py` 正确调用 `tier()` |
| 续跑约束 A/B | 两实例 18318（开）/ 18319（关） | 都能一次跑完；约束无副作用（不伪造工具调用），价值在"倾向收尾"的回合 |

### reasoning 路径

目录模型里只有 `glm-5.3` 会吐 `reasoning_content`（`deepseek-v4.1-flash`、`deepseek-v4-pro`、`kimi-k2-thinking` 都不吐），复现/回归必须用它：

```
item.added(0, reasoning, summary=[]) → part.added → text.delta×N
→ text.done / part.done / item.done(0, summary 有正文, 无 encrypted_content)
→ item.added(1, message) → completed(output=[reasoning, message], reasoning_tokens=29)
```

非流式同序（`output[0]=reasoning`），工具调用路径未受影响（`output[0]=function_call`，参数 `{"city":"北京"}` 正确）。

## 8. 兼容性与风险

| 风险 | 说明 | 缓解 |
|---|---|---|
| 既有接口回归 | 未改动 `openai.rs` / `anthropic.rs` 的请求路径，仅新增模块与路由 | smoke 覆盖三种协议；`cargo test` 全绿 |
| 续跑提示影响普通对话 | 无 `tools` 时不插入 | 仅在 `tools` 非空且未显式关闭时生效 |
| 内存历史泄漏 | `previous_response_id` 用进程内 `VecDeque`，上限 64 | 有界队列，只存过滤后的 message；不落盘 |
| 上游行为依赖 | 依赖"上游只在流式 chat 里回 `usage`""工具调用以 DSML 文本下发" | 两者都有兜底：无 usage 时填 0；DSML 与结构化调用合并去重 |
| Codex 版本漂移 | 工具类型集合可能变化（`namespace` 等） | 未知类型直接丢弃而非报错，客户端至少不会崩；补桥接见 §10 |

**回滚**：新增模块与路由相互独立，回滚只需还原 `src/api/mod.rs` / `src/api/admin.rs`（各 1 处）并删 `src/api/responses.rs`，不涉及数据迁移。

## 9. Reviewer 指南

按这个顺序读，20 分钟能覆盖全部关键路径：

1. `src/api/responses.rs` 的 `response_impl`（请求翻译 → 上游 → 分流）；
2. `build_messages` + `push_input_item`（协议核心，注意并行调用合并）；
3. `convert_tools` / `custom_tool_as_function` / `payload()`（工具类型保真，最易踩坑）；
4. `stream_body`（事件序列与索引，注意 reasoning/message/tool 的 `output_index` 推导）；
5. `docs/responses-api.md` §3 / §7 / §8（设计取舍、回合语义、已知缺口）。

## 10. 后续工作（不在本 PR 范围）

按"影响面 × 成本"排序，均已在 `docs/responses-api.md` §8 给出落点：

1. `namespace` 工具桥接（Codex 的 MCP 工具载体，当前整包丢弃 → MCP 工具不可用）；
2. 长思考期的 keepalive（`response.in_progress`），避免客户端 idle 超时；
3. `finish_reason=length` → `response.incomplete`（当前恒报 `completed`，截断被当成说完）；
4. `tool_choice` 在工具被过滤后仍转发导致上游 400；
5. `previous_response_id` 落 SQLite（当前进程重启即失效）。

## 11. Checklist

- [x] 新增能力有单测（17 项）且全量 `cargo test` 通过（83）
- [x] smoke 覆盖新端点（非流式 + 流式）
- [x] 真实客户端端到端验证（多 turn、多补丁、自跑测试）
- [x] 既有接口未改行为（smoke 三种协议全 PASS）
- [x] 新增环境变量有文档（`WB_RESPONSES_NUDGE`）
- [x] 文档与代码同步（`docs/responses-api.md`、README 目录树）
- [ ] 待补：`namespace` / keepalive / `incomplete`（见 §10）

---

## 附录 A：提交拆分（已落地）

```
chore: 项目基线（Responses 兼容层之前）          ← main
feat(api): 新增 OpenAI Responses API 兼容层      ← 1758+ / 3 文件
test(api): smoke 覆盖 responses 流式与非流式      ← +21
docs: 新增 Responses 实现手册与 PR 描述           ← +935 / 2 文件
chore(scripts): 新增 restart.sh                  ← +85
```

## 附录 B：仓库状态

分支已推送到 fork：`dizhiwangawa-design/reverse-proxy`，基线为上游 `fe9c6d5`（`feat: recover structured tool calls from the upstream's in-band text`），其上 4 个提交。

```bash
git log --oneline fe9c6d5..feat/responses-api   # 4 个提交
git diff --stat fe9c6d5..feat/responses-api     # 9 文件 / +2834 行
```

`git diff` 即本 PR 的完整改动；基线本身可编译（`cargo check`），提交后 `cargo test` 83 项通过。

PR 描述可直接使用本文件第 1–11 节（去掉附录）。
