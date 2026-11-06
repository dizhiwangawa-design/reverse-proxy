# Responses API 兼容层：实现手册

> 面向本项目 `workbuddy-proxy`，但协议部分对所有「Responses 客户端 → Chat Completions 上游」的桥接都成立。
> 代码基准：`src/api/responses.rs`（1606 行）、`src/inband.rs`、`src/api/openai.rs`；`cargo test` 81 项。
> 参考实现对照见文末第 11 节。

---

## 0. 一页速览

```
Codex CLI / Agents SDK                workbuddy-proxy                     上游 copilot 网关
─────────────────────                 ────────────────────               ───────────────────
POST /v1/responses                    responses::create                  POST /v1/chat/completions
  instructions / input[]   ──────────▶ build_messages()  → messages[]  ──▶ 只有流式 chat
  tools[] (function/custom) ────────▶ convert_tools()    → tools[]        不支持结构化 tool_calls
  max_output_tokens        ──────────▶ max_tokens                        tool call 藏在 content 里（DSML）
  text.format              ──────────▶ response_format
                                      chat_with_rotation()  ◀──────────── SSE chunk（含 DSML 文本）
  response.*  事件族        ◀───────── stream_body() / 聚合 → output[]
```

三条铁律，违反任何一条客户端就"看起来能用、实际不干活"：

1. **工具类型必须原样还原**。客户端按 item 类型分发：Codex 只认 `custom_tool_call` 去跑 `apply_patch`，你回 `function_call` 它静默丢弃。
2. **回合结束由"有没有工具调用"决定**。响应里没有 tool call，客户端就认为任务完成，把控制权交回用户。
3. **流必须收尾**。`response.completed` / `response.failed` / `response.incomplete` 必须发出一个，否则客户端重连重试，重试耗尽后任务中止（表现为"要手动输入继续"）。

---

## 1. 两套协议差在哪

| 维度 | Chat Completions | Responses |
|---|---|---|
| 输入 | `messages[]`，content 是字符串或 part 数组 | `instructions` + `input[]`，input 里混着 message / function_call / *_call_output / reasoning 等 item |
| 输出 | 一条 `choices[0].message` | `output[]` 数组，可同时含 message、function_call、custom_tool_call、reasoning |
| 工具调用 | 结构化 `tool_calls[]`（name + JSON 字符串 arguments） | 每种工具类型一个 item 类型；`custom` 工具的载荷是**自由文本**（lark/regex 语法约束） |
| 工具集合 | 只有 `function` | `function` / `custom` / `namespace` / `tool_search` / `local_shell` / `web_search` / `computer_use` |
| 流式 | `data: {...delta...}` + `[DONE]` | 带名字的事件族 `response.created` … `response.completed`，每个事件自带 `type` |
| 会话 | 无状态，客户端每次带全量 messages | 可选 `store` + `previous_response_id` 引用上一轮 |
| token 统计 | `usage.prompt_tokens / completion_tokens` | `usage.input_tokens / output_tokens`，另有 cached / reasoning 明细 |

桥接层的本质：**把 item 化的世界压平成 message 化的世界，再把压平过程中丢失的类型信息带回响应**。丢失的类型信息 = 工具类型，这是第一个、也是最贵的坑。

---

## 2. 请求翻译（Responses → Chat）

入口：`responses::create` → `response_impl` → `build_messages`。

### 2.1 字段映射

| 来源 | 目标 | 说明 |
|---|---|---|
| `instructions` | `messages[0] = {role:"system"}` | 空串丢弃；客户端系统提示通常放这里 |
| `input: "文本"` | `{role:"user", content:"文本"}` | 简写形式 |
| `input: [item...]` | 逐项展开 | 见 2.2 |
| `max_output_tokens` | `max_tokens` | o 系列模型要改 `max_completion_tokens`（本项目未做，见第 8 节） |
| `text.format` | `response_format` | `json_schema` / `json_object` 两种 |
| `temperature` / `top_p` / `user` / `parallel_tool_calls` | 同名 | 原样透传 |
| `tools[]` | `tools[]` | 必须重排结构，见第 3 节 |
| `tool_choice` | `tool_choice` | 字符串或 `{type:"function",function:{name}}` |
| `stream` | 上游恒为 `true` | 上游只有流式实现，非流式由本层聚合 |
| `stream_options` | **删除** | 本上游不接受该参数；它在末尾 chunk 里本来就会带 `usage`，删了不丢统计 |
| `include` / `store` / `truncation` / `reasoning` / `metadata` | 丢弃 | 上游无对应概念；`reasoning.effort` 可映射成 `reasoning_effort`（未做） |
| `previous_response_id` | 展开历史 | 见第 6 节 |

### 2.2 input item → message 的对照

```rust
// src/api/responses.rs:417  push_input_item()
"message"                                  → {role, content}      // role: developer → system
"function_call" | "local_shell_call"       → 追加到上一条 assistant 的 tool_calls[]
"custom_tool_call"                         → arguments = {"input": <原始文本>}
"function_call_output" |
"custom_tool_call_output" |
"local_shell_call_output"                  → {role:"tool", tool_call_id, content}
"reasoning"                                → 丢弃（上游无状态）
其它带 text 的 item                        → {role:"user", content:text}
```

三个必须注意的实现细节：

1. **连续的工具调用要合并进同一条 assistant 消息**。chat 协议要求一轮里的多个 call 挂在同一个 `tool_calls[]` 下，否则上游解析出错。`push_assistant_call` 用 `messages.last_mut()` 判断末尾是否已是"无内容且有 tool_calls 的 assistant"，是则 push 进去（注意 `call.clone()`，直接 move 会所有权报错）。

```rust
// src/api/responses.rs:491
if last.get("role") == Some("assistant")
    && last.get("content").is_none_or(Value::is_null)
    && last.get("tool_calls").and_then(Value::as_array).is_some()
```

2. **`role: "developer"` 要降级成 `system`**。OpenAI 新规范用 developer，很多网关不认。本项目还有更硬的约束：上游网关对**小写 `developer`** 直接返回 `11-128 Illegal API invocation from an unapproved channel`，所以 `sanitize_chat_body()` 里连 `Developer`/`DEVELOPER` 的大小写都要照顾。

3. **content part 数组要按"有没有图"分流**。有图保留数组（`{type:"image_url"}`），纯文本压成字符串——上游对纯文本 part 数组的容忍度不如字符串。

### 2.3 空 tools / 空 tool_choice

客户端有时会带 `tools: []` 却仍带 `tool_choice`，或者 `tool_choice` 指向一个被过滤掉的工具。上游会 400。规则：

```rust
if chat_tools.is_empty() {
    // 不要带 tools，也不要带 tool_choice / parallel_tool_calls
}
```

本项目在 `response_impl` 里已经做到"空数组不插入 `tools`"，但 `tool_choice` 是无条件转发的——如果 `convert_tools` 把工具全过滤光了，这里会漏一个 `tool_choice`。属于待修项（第 8 节）。

---

## 3. 工具桥接：四种类型，两条通道

### 3.1 类型矩阵

| Responses 类型 | Chat 侧表达 | 回包 item | 事件通道 | 本项目 |
|---|---|---|---|---|
| `function` | `tools[].function` | `function_call` | `function_call_arguments.*` | ✅ |
| `custom` | 只收一个 `input: string` 的 function | `custom_tool_call` | `custom_tool_call_input.*` | ✅ |
| `namespace` | 展平成 `ns__tool` 的 function | `function_call` + `namespace` 字段 | `function_call_arguments.*` | ❌ 整包丢弃 |
| `tool_search` | 代理 function `tool_search` | `tool_search_call` | `function_call_arguments.*` | ❌ |
| `local_shell` | function | `local_shell_call` | 同 function | 半（请求侧认，回包退化成 `function_call`） |
| `web_search` / `computer_use` | function | `web_search_call` / `computer_call` | 同 function | ❌ |

`namespace` 是 Codex 给每个 MCP server 建的分组，子代理工具走 `collaboration` 命名空间。**把 namespace 整包丢掉 = 客户端的全部 MCP 工具凭空消失**，模型只会在文本里"假装调用"。

### 3.2 custom 工具：包装与还原

Codex 的 `apply_patch` 长这样（只有带 `model_catalog_json` 的完整配置才会声明，最简配置下 Codex 只给 `exec_command`）：

```json
{
  "type": "custom",
  "name": "apply_patch",
  "description": "...",
  "format": { "type": "grammar", "syntax": "lark", "definition": "start: begin_patch hunk+ end_patch ..." }
}
```

Chat 协议没有 freeform 概念，本项目把它伪装成单字符串 function：

```rust
// src/api/responses.rs:660  custom_tool_as_function()
{"type":"function","function":{
  "name": name,
  "description": 原描述 + "把完整的调用载荷原样放进 `input` 字段…" + lark 语法原文,
  "parameters": {"type":"object",
    "properties":{"input":{"type":"string","description":"工具载荷原文（free-form）"}},
    "required":["input"],"additionalProperties":false}}}
```

回包时再拆掉包装：

```rust
// src/api/responses.rs:804  Call::payload()
if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&self.arguments) {
    if let Some(Value::String(input)) = map.get("input") { return input.clone(); }
}
self.arguments.clone()   // 模型没按包装来、直接吐原文时也照样能用
```

`payload()` 的双分支不是"防御性编程"而是**必需**：模型经常无视 `{"input": ...}` 直接输出 `*** Begin Patch\n...`，此时 `arguments` 本身就不是合法 JSON，必须原样透传。

### 3.3 为什么"丢工具"是最贵的坑

真实故障链（本项目踩过）：

```
convert_tools 只保留 type=="function"
        ↓  apply_patch 被丢弃
模型从 17k 系统提示里"知道"自己要调 apply_patch，于是凭空生成一个 function_call
        ↓
本层按 function_call 回包
        ↓
Codex 只认 custom_tool_call，静默丢弃 → 文件没变
        ↓
模型看输出："我刚说要改，怎么没改？" → 再说一遍 → 死循环
```

用户视角就是"它只会讲、不会改""改一半就停"。**症状在客户端，根因在工具类型**。

### 3.4 namespace 展开的正确做法（待实现）

```rust
// 请求侧
for sub in tool.tools {
    let flat = format!("{}__{}", tool.name, sub.name);
    let chat = if flat.len() > 64 { sha256_hex(&flat)[..64].to_string() } else { flat };
    chat_tools.push(function_spec(chat, sub.parameters));
    map.insert(chat, (tool.name.clone(), sub.name.clone()));
}
// 回包侧
if let Some((ns, real)) = map.get(&call.name) {
    item["name"] = real; item["namespace"] = ns;
}
```

64 字符上限要用 sha256 截断而不是简单截断，否则 `a__b` 和 `a__b__c` 可能撞车。

---

## 4. 响应翻译（Chat → Responses）

### 4.1 非流式

上游照样是流式，本层把整段 SSE 读完聚合（`ChatAggregate::apply`），再一次性构造 JSON：

```json
{"id":"resp_...","object":"response","created_at":1730000000,"status":"completed",
 "model":"...",
 "output":[
   {"id":"msg_...","type":"message","status":"completed","role":"assistant",
    "content":[{"type":"output_text","text":"...","annotations":[]}]},
   {"id":"fc_...","type":"custom_tool_call","status":"completed",
    "call_id":"call_...","name":"apply_patch","input":"*** Begin Patch\n..."}
 ],
 "usage":{"input_tokens":123,"input_tokens_details":{"cached_tokens":0},
          "output_tokens":45,"output_tokens_details":{"reasoning_tokens":0},
          "total_tokens":168},
 "error":null, "incomplete_details":null, "store":false, ...}
```

`base_response()` 把所有字段都填上（哪怕是 `null` / 默认值）。原因：有些客户端用强类型结构体反序列化，缺字段直接 panic/报错，比填错更麻烦。

**空 output 必须兜底**：上游只回了 reasoning、或者只回了一个空 tool call 时，`output` 会是 `[]`，而部分客户端无条件读 `output[0]`。本项目补一条空 message item。

### 4.2 流式事件序列

纯文本：

```
response.created
response.in_progress
response.output_item.added        (message, status=in_progress)
response.content_part.added
response.output_text.delta  × N
response.output_text.done
response.content_part.done
response.output_item.done         (message, status=completed, 完整文本)
response.completed                (携带 output 全量 + usage)
```

工具调用（在文本之后）：

```
response.output_item.added        (custom_tool_call / function_call, payload 为空)
response.custom_tool_call_input.delta   ← custom 专用
response.custom_tool_call_input.done    （字段名是 input）
   —— 或 ——
response.function_call_arguments.delta
response.function_call_arguments.done   （字段名是 arguments）
response.output_item.done         (完整 item)
response.completed
```

要点：

- **`output_item.added` 里载荷给空串**，真正的载荷走 delta/done。原因：added 事件先于参数确定（尤其是回收出来的调用），给空串不会让客户端提前 dispatch。
- **`output_index` 必须与最终 `output[]` 的下标一致**，文本占 0，工具从 1 开始（`base_index = output.len()`）。
- **delta 只是 UI 用**。Codex 的 core 在 `OutputItemDone` 才真正执行工具（`handle_output_item_done`），delta 里的半截参数它只喂给"参数预览 diff"。所以 done 事件里的 item 必须是完整、可执行的。
- **必须发 `response.completed`**。缺它 → 客户端 idle 计时器超时 → 重连 → 重试耗尽 → 用户必须手打"继续"。
- **顺手把 keepalive 插进这条序列**。上面是最短序列；真实上游思考时，两个事件之间可能静默几十秒，需要在中间周期补 `response.in_progress`（见 8.2），否则客户端 300s 后判死。

### 4.3 reasoning 透传：真 item，但不伪造密文

上游把思考放在 `delta.reasoning_content`（DeepSeek/Kimi/GLM 系），只有纯文本，没有任何可解密的密文。三种兼容边界里本项目采用第一种：

| 方案 | 做法 | 判断 |
|---|---|---|
| **① 真 item + 不伪造密文**（采用） | 发完整 `reasoning` item 与 summary 事件，`encrypted_content` 字段直接**不存在** | 诚实：客户端不会拿到一个我们自己都解不开的 blob |
| ② 只发摘要事件 | 不发 item，只发 `reasoning_summary_text.delta` | 最容易被 SDK 判错：事件 `item_id` 没有对应 item |
| ③ 自定义可逆存档 | 自建"思考→id"存储，多轮回传时还原 | 超出上游能力：代理重启/多实例即断，收益为零 |

流式事件序列（reasoning 永远排在最前，占 `output_index: 0`）：

```
response.output_item.added              (type=reasoning, summary=[])
response.reasoning_summary_part.added   (summary_index=0)
response.reasoning_summary_text.delta   × N
response.reasoning_summary_text.done
response.reasoning_summary_part.done
response.output_item.done               (type=reasoning, summary=[{type:summary_text,text}])
```

非流式则在 `output[]` 里放 `{"id":"rs_...","type":"reasoning","status":"completed","summary":[{"type":"summary_text","text":...}]}`，位置在 message 之前。

四条实现约束：

1. **`encrypted_content` 要"缺字段"而不是 `null` 或空串**。客户端看到字段存在就会在下一轮原样回传；本层 `push_input_item` 对 `reasoning` item 是丢弃的（chat 协议无处安放），于是变成"每轮回传、每轮丢弃"的假状态。
2. **索引要整体后移**：有 reasoning 时 `text_index = 1`、工具从 2 起；没有时照旧 0/1。事件里的 `output_index` 必须与最终 `output[]` 下标一致。
3. **只在文本之前建 reasoning item**。这些模型都是先思考后输出；若某个上游把 `reasoning_content` 混在正文之后，宁可不建 item（丢弃该 delta），也不要造出索引错乱的事件。
4. **reasoning item 在"第一段可见正文出现时"就关闭**，不是等流结束。否则事件序列会变成 `reasoning.delta… → message.added → reasoning.done`，item 交错；虽然按 `item_id` 还能对上，但校验严格的客户端会认为 `output_item.done` 的顺序与 `output[]` 不一致。实现上是在正文分支开头补一次"关闭 reasoning"，并在收尾处兜底（只有思考、没有正文的回合）。
5. **`include: ["reasoning.encrypted_content"]` 不返回 400**。Codex 默认就带这个字段，拒绝等于全盘失效；正确做法是"该字段不提供"，在语义上就是"我没有可交付的密文"。

实测（`glm-5.3`，唯一会吐 `reasoning_content` 的目录模型）：

```
response.output_item.added          idx=0 type=reasoning  summary=[]
response.reasoning_summary_part.added  idx=0
response.reasoning_summary_text.delta  × N
response.reasoning_summary_text.done   idx=0
response.reasoning_summary_part.done   idx=0
response.output_item.done           idx=0 type=reasoning  summary=[{type:summary_text,text:...}]  （无 encrypted_content）
response.output_item.added          idx=1 type=message
...
response.completed                  output=[reasoning, message]，usage.output_tokens_details.reasoning_tokens=29
```

### 4.4 usage

```rust
prompt_tokens      → input_tokens
completion_tokens  → output_tokens
prompt_tokens_details.cached_tokens       → input_tokens_details.cached_tokens
completion_tokens_details.reasoning_tokens→ output_tokens_details.reasoning_tokens
total = input + output
```

注意不要靠 `stream_options.include_usage` 求 usage：本上游不接受该参数（`openai.rs:145` 处直接 remove），但它会在末尾 chunk 自带 `usage`，照常读即可。

---

## 5. 工具调用回收：上游把调用写进了正文

这个上游（以及不少国内中转）虽然接受 `tools`，但**不返回结构化 `tool_calls`**——每个 delta 的 `tool_calls` 都是空数组，调用以文本形式塞在 `content` 里：

```
<｜｜DSML｜｜ calls>
<｜｜DSML｜｜ invoke name="read">
<｜｜DSML｜｜ parameter name="path" string="true">/etc/hostname</｜｜DSML｜｜ parameter>
</｜｜DSML｜｜ invoke>
</｜｜DSML｜｜ calls>
```

`TextToolCallScanner` 做增量解析，三个必须满足的性质：

1. **任意切分点安全**。SSE 会在 tag、属性、载荷中间断开，扫描器保留"可能是标记开头"的尾部字节，等后续 delta 补齐。
2. **`string="true"` 是字面文本，`string="false"` 是已编码 JSON**（数组/对象/数字/布尔原样拼接）。搞反了会把 `"a,b"` 拆成两个参数。
3. **流结束时 `finish()` 兜底**。闭合标签没来（被截断）也要把已累积的调用吐出来，否则这一轮的动作全丢。

回收出来的调用与结构化的调用要**去重合并**（按 `(name, arguments)`），因为有些上游两种都给：

```rust
// src/api/responses.rs:1352  recover_calls()
let mut seen: BTreeMap<(String, String), ()> = BTreeMap::new();
```

并且**每个调用重新分配 `call_id`**（`call_{uuid}`）：客户端会把 call_id 原样回传在 `*_call_output` 里，如果两次响应撞了同一个 id，历史就串了。

---

## 6. 会话状态与 previous_response_id

Codex 每轮都带全量 `input`，所以对本项目几乎无感；但 Agents SDK、部分客户端依赖 `previous_response_id`。

本层实现（`HISTORY_LIMIT = 64`，进程内 `Mutex<VecDeque<(id, messages)>>`）：

```
无 input + 有 previous      → messages = history(previous)
有 input + 有 previous      → [system 前缀] + history(previous) + 新输入
找不到 previous             → 忽略（不报错）
```

拼接时要把"新请求的 system 前缀"提到最前面，否则 instructions 会被淹在历史里。

局限与改进方向（借鉴 codex-shim / codeMUX）：

- 进程重启即失效 → 落 SQLite（`response_store.sqlite`），Codex 侧重启也能续。
- 只按 response_id 索引 → 增加 `call_id → response_id` 反查，客户端只带 `*_call_output` 而不带 response_id 时也能补回 call item。
- `GET /v1/responses/{id}` 目前恒 404（明确告知不持久化），比返回空壳更安全。

---

## 7. 编码代理的回合语义（"改一半就停"）

### 7.1 判定规则

Codex 的循环：发请求 → 拿到 output → 逐个 item 执行 → **如果这一轮没有任何工具调用，turn 结束**。所以"模型只输出散文"在协议上等价于"任务完成"。

真实会话日志（用户侧）：

```
15:19:29 [custom_tool_call] apply_patch  → Success. Updated ...
15:19:33 [function_call]    exec_command (rg ...)
15:19:36 [message/assistant] "README 已加入说明；我正在清理…"
15:19:36 <task_complete>
```

补丁确实生效了，但它说完"我正在…"就收尾。

### 7.2 解法：续跑约束（本项目已实现）

`build_messages` 在**最后一条 user 消息之后**插入一条 system 消息（放最后会破坏 chat 的消息顺序约束，放这里是"离模型最近又不越界"的位置）：

```rust
// src/api/responses.rs:381
if has_tools(obj) && nudge_enabled() {
    let last_user = messages.iter().rposition(|m| m["role"] == "user");
    let nudge = json!({"role": "system", "content": CONTINUATION_NUDGE});
    match last_user { Some(i) => messages.insert(i + 1, nudge), None => messages.push(nudge) }
}
```

文案五条，对应五种真实失效模式：

| 条款 | 针对的失效 |
|---|---|
| 未完成必须带工具调用 | 说完就停 |
| 不要"我正在…"顶替动作 | 用叙述冒充执行 |
| 验证通过才可输出纯文字总结 | 提前宣布完成 |
| 一次改不完连续调，不要征求意见 | 频繁请示、把回合交回人 |
| 补丁失败要拆小 hunk 重试 | "补丁因匹配范围过大被中断"后放弃 |

开关：`WB_RESPONSES_NUDGE=0`（或 false/no/off）。**保留开关是必须的**——它是唯一能在不重新编译的情况下做 A/B 的手段。

### 7.3 同类症状的其它根因（别一律归给模型）

| 现象 | 根因 | 判据 | 修法 |
|---|---|---|---|
| 说"我正在…"就停 | 模型把回合交回 | 日志里 `task_complete` 紧跟纯文本 message | 续跑约束 |
| 停下来但报 `Reconnecting...` | 流被中断、重试耗尽 | rollout 里有 `stream disconnected` | 代理必须发 `response.completed`；客户端侧调大重试预算（见 7.5） |
| 输出完什么都没改 | 工具类型被丢（第 3.3 节） | 日志里 `function_call` 后面没有对应的 `*_output` | 还原 `custom_tool_call` |
| 长时间无输出后停 | 上游思考期没有事件，客户端 idle 超时（Codex 约 300s，且不认 SSE 注释行） | 日志里两轮请求间隔 > 300s | 周期性发 `response.in_progress` 当 keepalive（见 8.2） |
| 输出被截断却"正常结束" | `finish_reason=length` 被当成 `completed` | 上游 finish_reason 是 `length`，输出 item 停在半句 | 发 `response.incomplete`（见 8.3） |

最后两条是**本项目当前缺口**：上游长时间思考时本层一个事件都不发，长任务容易被客户端判死；被 `max_tokens` 截断时又谎报"完成"。

### 7.5 客户端侧兜底：把重试预算调够（codex-keeper 的做法）

代理侧只能保证"事件收尾正确"，但如果链路本身会断（中转抖动、容器网络），重试预算太小仍然会"重试耗尽 → 任务中止 → 要手打继续"。codex-keeper 的思路是把预算拉到远超偶发抖动：

```toml
# ~/.codex/config.toml —— 对每个 model_provider 都设
[model_providers.workbuddy]
request_max_retries    = 40     # 默认 4，太容易耗尽
stream_max_retries     = 100    # 默认 5
stream_idle_timeout_ms = 120000 # 默认 300000；有 keepalive 后可保持 120s
```

对应到本项目：代理发 keepalive 之后，可以把 `stream_idle_timeout_ms` 压到 60–120s，让"真死了"更快暴露，而不是干等 5 分钟。

> 顺序很重要：**先修代理（事件必须收尾 + keepalive），再调客户端重试**。只调重试预算等于给漏水的桶加更大的桶。

### 7.4 对照实验（怎么证明改动有效）

```bash
# A 组：约束开（默认）
WB_PROXY_BIND=127.0.0.1:18318 WB_DATA_DIR=$PWD/data ./target/release/workbuddy-proxy &
# B 组：约束关
WB_RESPONSES_NUDGE=0 WB_PROXY_BIND=127.0.0.1:18319 WB_DATA_DIR=/tmp/data-off ./target/release/workbuddy-proxy &

# 同 prompt 各跑一次，比较"一个 turn 内完成的步骤数 / 是否需要人工催"
CODEX_HOME=/path/to/codex-a codex exec --json -s workspace-write --skip-git-repo-check \
  -C /tmp/proj "六步重构：..." </dev/null > a.jsonl
```

统计口径：`item.completed` 里 `command_execution` / `file_change` 的条数、`turn.completed` 是否在一次内出现、最终产物是否通过测试。

实测结论（deepseek-v4.1-flash）：简单任务与六步重构，开/关都能一次跑完；约束没有副作用（不会伪造工具调用），价值在于兜住"倾向收尾"的回合。

---

## 8. 已知缺口与改进清单

按"影响面 × 实现成本"排序，每条都给了落点。

| # | 缺口 | 症状 | 落点 | 参考做法 |
|---|---|---|---|---|
| 1 | `namespace` 工具被丢弃 | MCP 工具全不可用，模型假装调用 | `convert_tools` 展开 + `Call` 增加 namespace | `ns__tool` 扁平 + sha256 截 64（codeMUX） |
| 2 | 无 keepalive | 长思考被客户端判超时 | `stream_body` 的 `while let` 里加定时分支，心跳发 `response.in_progress` | 空闲 15–30s 发一次；Codex 不认 SSE 注释，必须真事件 |
| 3 | `finish_reason="length"` 未映射 | 输出被截断却报 `completed`，客户端以为说完了 | `stream_body` 收尾处判断 `agg.finish_reason` | 发 `response.incomplete` + `incomplete_details.reason="max_output_tokens"` |
| 4 | `tool_choice` 可能指向已过滤工具 | 上游 400 | `response_impl` 里 tools 为空时删 `tool_choice` | codeMUX 的等价修法 |
| 5 | `previous_response_id` 不持久 | 代理重启丢上下文 | 换 SQLite 存储 | codex-shim 的 `response_store.sqlite` |
| 6 | o 系列模型 `max_tokens` 报错 | 400 | 模型名以 `o1/o3/o4` 开头时换 `max_completion_tokens` | codeMUX |
| 7 | 不认 `/v1/v1/responses` | base_url 多带一层 `/v1` 就 404 | 加兼容路由 | 很多中转都这么兜 |
| 8 | 图片发给纯文本模型 | 上游媒体错误 | 检测模型能力，降到文本占位符 | codeMUX 的 `applyMediaPrevention` |
| 9 | WebSocket 传输未实现 | 客户端开 `responses_websocket` 后握手失败 | 至少返回明确 426/400，或实现 `response.create` + `response.interrupt` | codex-router PR #1004 |
| 10 | ~~reasoning item 不完整~~ **已实现** | 原先 summary 事件的 `item_id` 指向 message item，且 output 里没有 reasoning item | `reasoning_item()` + `stream_body` 索引重排 | 见 4.3；不发 `encrypted_content` |

### 8.1 namespace 桥接（#1，影响最大）

Codex 把每个 MCP server 的工具塞进一个 namespace 包：

```json
{"type":"namespace","name":"mcp__docs","tools":[
  {"type":"function","name":"search","description":"...","parameters":{...}},
  {"type":"function","name":"fetch","description":"...","parameters":{...}}]}
```

Chat 侧没有分组概念，只能展平。落地要点（`convert_tools` 返回值从 `(Vec<Value>, Vec<String>)` 升级成 `(Vec<Value>, ToolCtx)`）：

```rust
struct ToolCtx {
    /// chat 里的扁平名 → (namespace, 真实名, kind)
    chat_to_spec: HashMap<String, Spec>,
}
struct Spec { namespace: Option<String>, name: String, kind: Kind }
enum Kind { Function, Custom }

// 请求侧
for sub in tool["tools"].as_array().into_iter().flatten() {
    let flat = format!("{}__{}", ns, sub_name);
    let chat_name = if flat.len() > 64 {
        // 必须哈希而不是截断：a__b 与 a__b__c 截断后会撞车
        sha256_hex(&flat)[..64].to_string()
    } else { flat };
    chat_tools.push(function_spec(&chat_name, sub));
    chat_to_spec.insert(chat_name, Spec { namespace: Some(ns), name: sub_name, kind: Kind::Function });
}

// 回包侧：名字换回真名，并补 namespace 字段
if let Some(spec) = ctx.chat_to_spec.get(&call.name) {
    item["name"] = spec.name.clone();
    if let Some(ns) = &spec.namespace { item["namespace"] = ns.clone(); }
}
```

两个容易忽略的点：

- **`Call.custom: bool` 建议升级为 enum**。加上 namespace 后就有三种回包形态，三四个并列 bool 很快会互相打架。
- **name 冲突**：不同 namespace 下同名工具展平后靠前缀区分，但模型可能把 `mcp__docs__search` 抄错成 `search`。回包侧要做一次宽松匹配（去前缀再比），否则调用直接失效。

### 8.2 keepalive（#2）

`stream_body` 现在是纯 `while let Some(chunk) = upstream.next().await`，上游思考期间链路上一个字节都没有。加一个空闲计时分支即可：

```rust
let mut last_event = Instant::now();
loop {
    let tick = tokio::time::sleep(Duration::from_secs(10));
    tokio::select! {
        chunk = upstream.next() => { /* 原来的处理；每次产出事件后 last_event = Instant::now() */ }
        _ = tick => {
            if last_event.elapsed() >= Duration::from_secs(20) {
                // 必须是真事件：Codex 跳过 SSE 注释行（": ping"），不重置 idle 计时
                yield Ok::<Bytes, std::io::Error>(event(
                    "response.in_progress",
                    json!({"type": "response.in_progress",
                           "response": base_response(&response_id, &model, created, "in_progress", json!([]), Value::Null)}),
                ));
                last_event = Instant::now();
            }
        }
    }
}
```

细节：

- 间隔取 **10–20s**。太密浪费带宽和客户端渲染，太疏（>60s）压不住默认 300s 的 idle 计时。
- 不能用 `": keep-alive\n\n"` 注释行——很多实现这么写，但 Codex 的 idle 计时器只认真事件。
- 已经产出过输出 item 后再发 `in_progress` 是安全的（客户端会忽略空 output），但**不要**在 `response.completed` 之后发。

### 8.3 `incomplete`（#3）

截断必须在协议上说清楚，否则客户端把半句话当最终答案：

```rust
// stream_body 收尾处
let cut = matches!(agg.finish_reason.as_deref(), Some("length") | Some("max_tokens"));
let response = json!({
    "type": "response",
    "status": if cut { "incomplete" } else { "completed" },
    "incomplete_details": if cut { json!({"reason": "max_output_tokens"}) } else { Value::Null },
    // ... 其余字段同 base_response
});
yield Ok(event(if cut { "response.incomplete" } else { "response.completed" },
               json!({"type": if cut { "response.incomplete" } else { "response.completed" }, "response": response })));
```

非流式路径同样要改：`base_response(..., status, ...)` 的 `status` 现在是硬编码 `"completed"`。

顺带一个语义细节：`recover_inband_tool_calls()` 会把 `finish_reason` 为 `None` 或 `"stop"` 的情况改写成 `"tool_calls"`（`openai.rs:635`），`"length"` 不受影响。所以判断截断只认 `length` / `max_tokens`，并且要在回收之后读，否则"回收出工具调用"会被误判成截断。

---

## 9. 验证与排障手册

### 9.1 三层验证

```bash
# 1) 单元测试：协议转换的正确性
cargo test

# 2) smoke：路由/鉴权/三种协议都能出字
BASE=http://127.0.0.1:8317 ./scripts/smoke.sh

# 3) 真实客户端：只有这一步能证明"能干活"
CODEX_HOME=/tmp/codex-probe codex exec --json -s workspace-write \
  --skip-git-repo-check -C /tmp/proj "创建 a.py 并运行它" </dev/null
```

### 9.2 抓真实请求（最快定位工具类型问题）

不要靠猜客户端发了什么。起一个探针 HTTP 服务，把 `base_url` 指过去，落盘 `requests.jsonl`：

```python
# /mnt/windata/wbprobe/server.py 的骨架
import json, http.server
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        open("requests.jsonl", "a").write(body.decode() + "\n")
        self.send_response(200); self.send_header("Content-Type","text/event-stream"); self.end_headers()
        # 回一个最小合法 SSE：created → completed
    def log_message(self, *a): pass
```

然后：

```bash
python3 -c "
import json
for l in open('requests.jsonl'):
    b=json.loads(l)
    print([ (t.get('type'), t.get('name')) for t in b.get('tools',[]) ])
"
```

看到 `[('custom','apply_patch'), ('function','exec_command'), ('namespace','mcp__xxx'), ...]` 就说明必须做第 3 节的桥接。

### 9.3 看客户端侧发生了什么

Codex 的 rollout 文件（`$CODEX_HOME/sessions/**/rollout-*.jsonl`）里逐条 item 都在。常用 grep：

```bash
rg -n 'custom_tool_call|function_call|task_complete' rollout-*.jsonl | tail -30
```

判据：`*_call` 后面有没有配对的 `*_output`。没有 = 客户端丢弃了这次调用。

### 9.4 本机沙箱的三个坑（与协议无关但会浪费一小时）

| 坑 | 现象 | 处理 |
|---|---|---|
| `http_proxy` 劫持 localhost | `curl 127.0.0.1:8317` 报 `Connection refused` / 走代理 | `curl --noproxy '*'`，脚本里 `export no_proxy=127.0.0.1,localhost` |
| `codex exec` 后台运行卡住 | 打印 `Reading additional input from stdin...` 不动 | 必须 `</dev/null` |
| `nohup ... &` 起的进程被回收 | 刚起的代理几秒后消失 | 用前台 `exec` 放进常驻任务；`setsid` 也不一定活 |

---

## 10. 代码地图与扩展步骤

```
src/api/responses.rs
├── create / get / delete            路由入口、错误记录
├── response_impl                    请求翻译 → 上游 → 分流（流式/聚合）
├── build_messages                   协议核心：input[] → messages[]
│   ├── push_input_item              各种 item 的落地
│   ├── push_assistant_call          合并并行调用
│   └── CONTINUATION_NUDGE           续跑约束（第 7 节）
├── convert_tools / custom_tool_as_function   工具桥接（第 3 节）
├── convert_tool_choice / convert_text_format
├── map_usage / base_response        字段与 token 映射
├── stream_body                      SSE 事件序列（第 4.2 节）
│   └── recover_calls                去重合并 + 重发 call_id
└── tests                            17 个协议级单测
src/inband.rs                        DSML 文本工具调用回收（第 5 节）
src/api/openai.rs                    ChatAggregate、chat_with_rotation、流式错误提取
src/sanitize.rs                      developer 角色 / 特征串清洗（上游 11-128 规避）
```

### 加一种工具类型（checklist）

1. `convert_tools` 里认 `type`，产出 chat function 规格，并登记 `name → kind`。
2. `Call` 增加 kind 字段（现有 `custom: bool` 建议升级为 enum，避免三四个 bool）。
3. `tool_item()` 按 kind 造 item（字段名可能不同：`arguments` vs `input`）。
4. `stream_body` 里按 kind 选事件通道（`function_call_arguments.*` / `custom_tool_call_input.*`）。
5. `push_input_item` 补对应的 `*_call` / `*_call_output` 反向映射（客户端会把历史原样发回来）。
6. 加两个单测：请求侧包成什么样、回包侧还原成什么样。

### 加一个事件（checklist）

1. 事件必须有 `type` 字段，且 `event:` 行与 `data.type` 一致（有的客户端只读其中一个）。
2. `output_index` / `item_id` 与最终 `output[]` 对齐。
3. 事件顺序不能倒：`added` → `delta` → `done`。
4. 收尾三选一：`completed` / `failed` / `incomplete`，**必须有且只有一个**。

---

## 11. 开源实现对照

| 项目 | 语言 | 值得借鉴的点 |
|---|---|---|
| [chutesai/responses-proxy](https://github.com/chutesai/responses-proxy) | Rust/axum | 分片安全的工具参数缓冲；同时发新旧两套事件（`output_tool_call.*` + `function_call_arguments.*`）；`response.incomplete` 映射；请求体大小/工具数限额；熔断器 |
| [Cmochance/codex-app-transfer](https://cmochance.github.io/codex-app-transfer) | Rust/Tauri | apply_patch 桥接 + **畸形 V4A 补丁的非破坏性修复中间层**；`previous_response_id` 两层 LRU + SQLite（体积降 97%）；多账号池 + 配额守卫（明确列了 WorkBuddy/CodeBuddy 作为上游） |
| [jroth1111/codex-shim](https://github.com/jroth1111/codex-shim) | Python | 原生工具的 BYOK 退化表（`apply_patch`→`{patch}`、`local_shell`→`{command}`、`web_search`→`{query}`）；未知 `previous_response_id` 明确 404 而不是静默借用上下文；每个请求一条结构化 `[access]` 日志 |
| [kfumi/codeMUX 的 Codex 代理指南](https://github.com/kfumi/codeMUX/blob/master/docs/guides/codex-routing-proxy-guide.md) | Rust | 四种工具类型的双向映射表（function/custom/namespace/tool_search）；`ns__tool` 扁平 + sha256 截断；`tool_choice` 空数组陷阱；`/v1/v1` 双前缀兼容；o 系列 `max_completion_tokens` |
| [@classicicn/codex-transfer](https://socket.dev/npm/package/@classicicn/codex-transfer) | Node | 最小可用的协议翻译骨架，适合快速对照字段 |
| [codex-keeper](https://npmjs.com/package/codex-keeper) | Node | 断流治理：把 `request_max_retries` / `stream_max_retries` / `stream_idle_timeout_ms` 调大；交互模式自动代打"继续"，exec 模式 `resume --last` 接力 |
| [aimock 的 Responses 文档](https://aimock.copilotkit.dev/responses-api) | 文档 | 事件序列的权威样例；明确 Codex 的 `apply_patch` 是 custom 工具、MCP 工具走 namespace；fixture 里给出了 custom 工具的 `input` 形态 |

### 11.1 补丁修复层：值得抄，但现在不抄

`codex-app-transfer` 除了协议翻译，还在 custom tool 的调用链上加了一层"非破坏性修复"：模型吐出的 V4A 补丁（`*** Begin Patch` / `*** Update File:` 那一套）经常有小毛病——行号对不上、`@@` 上下文块少了抬头、`+`/`-` 前缀错位、尾随空行缺失。它不改原始补丁、只在内存里修补成可应用的形态再交给客户端。

什么时候才需要抄它：

- 出现"补丁调用成功回包、但客户端报 `Failed to apply patch`"的稳定复现；
- 或者模型反复在同一个 hunk 上失败、退化成"说明一下就算改完"。

现在还不需要：本项目的补丁链是 **代理透传原文 → Codex 自己 apply_patch**，代理没有任何机会（也不应该）改补丁内容；一旦代理开始"猜"补丁，反而可能出现"文件内容与模型认知不一致"的更糟状态。真要抄，位置在 `Call::payload()` 之后、发 `custom_tool_call_input.done` 之前，只能是"补齐明显缺失的补丁结构"这种无歧义修复，且必须打日志。

另外这个项目把 **WorkBuddy/CodeBuddy 明确列为一类上游**（多账号池 + 配额守卫 + 标注为 experimental），说明本项目的上游形态在社区里已被验证可行——它的多账号轮换和配额保护思路可以直接对照 `src/state.rs` 的实现。

一句话总结这些项目的共识：**协议翻译不难，难在类型保真和回合收尾**。凡是"看起来通了但不会干活"的，先查工具类型；凡是"干到一半停了"的，先查事件有没有收尾。



---

## 12. 排障决策树

```
客户端"不能用"
├─ HTTP 404 route not found        → 路由没实现（本项目最初就是 /v1/responses 404）
├─ 401/403                          → 鉴权头没透传，或 base_url 层级错
├─ 200 但界面无任何输出             → 事件没发或 JSON 缺字段；先 curl 看原始 SSE
├─ 有输出但不改文件
│   ├─ *_call 后无 *_output         → 工具类型被丢（第 3 节）
│   └─ 补丁调用有 output 但内容失败 → 补丁上下文/范围问题（续跑约束第 5 条）
└─ 输出一段就停
    ├─ 日志有 Reconnecting          → 流没收尾/被中断（第 7.3 / 7.5 节）
    ├─ 间隔 >300s 才断              → 缺 keepalive（第 8.2 节）
    └─ 干净地 task_complete         → 模型交还控制权（续跑约束，第 7.2 节）
```
