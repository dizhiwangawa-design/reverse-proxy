# 思考档位控制:开发指南

> 本文是 `/v1/responses` 与 `/v1/messages` 两条路径上"思考程度"的完整实现手册。
> 所有档位数据来自对上游 `GET /v2/enterprises/personal/models` 的实拉(37 个模型),
> 所有行为结论来自绕过代理直连上游的实测——同时读
> `usage.reasoning_tokens` 与 `delta.reasoning_content`,因为前者会漏报(见 §5.2)。
> 不是从 OpenAI / Anthropic 公开文档推断的:上游的档位阶梯与这两家都不一样。

## 0. 一句话结论

上游的思考档位**不是统一的**。37 个模型里存在 7 种不同的档位声明,所以
"一张全局映射表打天下"是错的;正确做法是**每个请求先查目标模型的档位集合,
再在该集合内归一化**。

---

## 1. 事实基础:先拿到上游的档位目录

### 1.1 为什么必须实测,不能照抄公开文档

三种来源的档位说法互相冲突:

| 来源 | 声称的档位 | 问题 |
| --- | --- | --- |
| OpenAI Responses 文档 | `minimal/low/medium/high`(+ `xhigh`) | 上游不支持 `minimal` 作独立档 |
| Anthropic Messages | `budget_tokens` 连续值 | 上游只认档位名,不认预算 |
| 上游模型目录 | **逐模型不同**(见 1.3) | 唯一权威 |

如果按 OpenAI 文档写死 `low/medium/high`,会出现两类静默错误:
对只支持 `low/high/max` 的模型,`medium` 语义不明;对支持 5 档的模型,
`medium` 被无谓地压到 `high`,凭空增加延迟和 token 成本。

### 1.2 怎么拉

```bash
# 目录接口(注意是 /v2/,不是 /v1/)
GET https://copilot.tencent.com/v2/enterprises/personal/models
Authorization: Bearer <access_token>
x-user-id: <account uid>          # 不带上可能拿到空列表
```

本仓库带一个可直接跑的脚本:

```bash
python3 scripts/dump-model-efforts.py            # 打印全部模型的档位表
python3 scripts/dump-model-efforts.py --summary  # 按档位集合分组
python3 scripts/dump-model-efforts.py --json out.json  # 存原始目录
```

它默认读 `./data/state.json`(可用 `WB_DATA_DIR` 覆盖),自动挑**有效期最长**的可用账号,
避免拿快到期的 token 去做目录查询。

脚本要点:

- `endpoint` 为空时回落 `https://copilot.tencent.com`;
- 返回体可能被包在 `data` / `models` / `list` / `result` 里,需要递归找数组;
- 空列表不是"没有模型",多半是漏了 `x-user-id` 头。

### 1.3 全量档位清单(2026-11 实测,37 个模型)

#### A. 标准三档 `low/high/max` — 5 个

| 模型 | defaultEffort | canDisableThinking |
| --- | --- | --- |
| `deepseek-v4.1-flash` | `high` | `true` |
| `kimi-k2.8-preview` | `high` | `true` |
| `glm-5.3` | `high` | `false` |
| `glm-5.3-flash` | `high` | `false` |
| `kimi-k3.1-pro` | `high` | `false` |

这是最常见的一类,也是早期实现唯一覆盖的一类。

#### B. 五档 `low/medium/high/xhigh/max` — 2 个

| 模型 | defaultEffort | canDisableThinking |
| --- | --- | --- |
| `space-bunny` | `max` | `false` |
| `minimax-m3.1-flash` | `max` | `false` |

**这一类的存在直接否定了"全局三档"的假设。** 它们本可以精确表达 `medium`
和 `xhigh`,若用三档表 clamp 会被压到 `high` / `max`——不是能力丢失,而是
**强制加深**:延迟和 token 成本都会涨。

#### C. 两档 `low/high` — 1 个

| 模型 | defaultEffort | canDisableThinking |
| --- | --- | --- |
| `hy3-x` | `high` | `false` |

#### D. 三档但含 `medium` `low/medium/high` — 1 个

| 模型 | defaultEffort | canDisableThinking |
| --- | --- | --- |
| `step-5-preview` | `high` | `false` |

注意:这一类的"中档"叫 `medium` 而不是 `high`,与 A 类的命名规则不同。

#### E. 单档 `high` — 3 个

| 模型 | defaultEffort | canDisableThinking |
| --- | --- | --- |
| `hy4free` | `high` | `false` |
| `hy4std` | `high` | `false` |
| `hy4-preview-x` | `high` | `false` |

只有一个档位,意味着**没有可调粗细**:传任何值效果相同(实测确认,见 §5.4)。

#### F. 单值 `reasoning.effort` — 16 个(占比最大)

这 16 个模型**没有 `supportedEfforts` 字段**,只有一个单值 `effort`:

| 模型 | `reasoning.effort` | | 模型 | `reasoning.effort` |
| --- | --- | --- | --- | --- |
| `auto` | `high` | | `kimi-k2-thinking` | `medium` |
| `balanced-model` | `medium` | | `kimi-k2.5` | `medium` |
| `deep-model` | `medium` | | `kimi-k3-1` | `medium` |
| `deepseek-v3-2-volc` | `medium` | | `mimo-v2.6-pro` | `high` |
| `deepseek-v4-flash` | `high` | | `minimax-m2.5` | `medium` |
| `deepseek-v4-pro` | `high` | | `glm-4.6v` | `medium` |
| `fast-model` | `medium` | | `glm-5.2` | `medium` |
| `hunyuan-2.0-thinking` | `medium` | | `hy3` | `high` |

这个字段**不是**"支持的档位",更可能是"当前生效值"或"唯一预设值"。
实测(§5.5)显示这些模型仍会响应 `reasoning_effort` 参数,但档位语义不清。
**不要把它当 supportedEfforts 用。**

#### G. 无 `reasoning` 对象 — 9 个

`default`、`glm-4.6`、`glm-4.7`、`glm-5.0`、`hunyuan-chat`、
`hunyuan-image-alpha`、`hunyuan-image-alpha-edit`、`kling-video-v3`、
`minimax-video-h3`

图像/视频/老对话模型。传 `reasoning_effort` 无意义,应当**不透传**,
否则可能触发上游 400。

### 1.4 汇总分布

`scripts/dump-model-efforts.py --summary` 的实际输出(按模型数降序):

```
[single:medium]             11 models   ← 单值声明,无 supportedEfforts
[-]                          9 models   ← 无 reasoning 对象
[single:high]                5 models   ← 单值声明
[high]                       3 models   ← 只支持一档
[low|high|max] canDisable=F  3 models
[low|high|max] canDisable=T  2 models
[low|medium|high|xhigh|max]  2 models   ← 五档,会被三档表降级
[low|high]                   1 model
[low|medium|high]            1 model
```

按声明形态归并:

| 形态 | 模型数 | 占比 |
| --- | --- | --- |
| `supportedEfforts` 数组 | 12 | 32% |
| 单值 `reasoning.effort` | 16 | 43% |
| 无 `reasoning` 对象 | 9 | 24% |

**只有 5/37 个模型是 `low/high/max` 三档。** 任何把它当全局规则的实现,
对另外 32 个模型都是猜的。

---

## 2. 档位语义模型

### 2.1 两条协议的输入形态不同

| 路径 | 客户端字段 | 语义 |
| --- | --- | --- |
| `/v1/responses` | `reasoning.effort: "low"` | 具名档位 |
| `/v1/messages` | `thinking.budget_tokens: 4096` | 连续预算 |

上游只接受具名档位(经实测,`thinking_budget` / `reasoning.max_tokens` /
`chat_template_kwargs` / `enable_thinking` 等候选参数族均无效,
只有 `reasoning_effort` 起作用)。所以 Anthropic 的预算必须**映射成档位**。

### 2.2 归一化规则

```rust
// src/api/responses.rs

/// 客户端可能送来的全部标签(case-insensitive)。
const RESPONSES_REASONING_EFFORTS: &[&str] =
    &["minimal", "low", "medium", "high", "xhigh", "max"];

/// 目标模型实际支持的档位。应由 §1.2 的目录动态提供。
const SUPPORTED_EFFORTS: &[&str] = &["low", "high", "max"];

/// 明确表示"不要思考"的字面量。
pub(crate) const REASONING_OFF: &str = "none";

pub(crate) fn clamp_effort(named: &'static str) -> &'static str {
    if SUPPORTED_EFFORTS.contains(&named) {
        return named;
    }
    match named {
        "minimal" | "low" => "low",
        "medium" | "high" => "high",
        _ => "max",
    }
}
```

### 2.3 为什么向上取而不是向下

档位阶梯是 `low < high < max`。给定 `medium`,两个候选是 `low` 和 `high`。

选 `high` 的理由:**"要求多想"被悄悄变成"少想"是更严重的错误**。
降级会让调用方在不知情的情况下拿到质量更低的回答,而且很难归因;
升级只是多花 token,质量下限有保证。

这条规则对 A 类模型成立。但对 B 类(5 档)模型,`medium` 本可以精确表达,
**不应被 clamp**——见 §2.5。

### 2.4 Anthropic 预算分桶

```rust
// src/api/anthropic.rs
fn reasoning_effort_from_thinking(obj: &Map<String, Value>) -> Option<&'static str> {
    let thinking = obj.get("thinking")?;
    let kind = thinking.get("type").and_then(Value::as_str).unwrap_or("enabled");
    if kind != "enabled" {
        return Some(crate::api::responses::REASONING_OFF);   // 显式关闭
    }
    let budget = thinking.get("budget_tokens").and_then(Value::as_i64).unwrap_or(0);
    let bucketed = match budget {
        i64::MIN..=4095 => "low",
        4096..=16383 => "high",
        _ => "max",
    };
    Some(crate::api::responses::clamp_effort(bucketed))       // 复用同一张表
}
```

分桶边界的选择依据是 Anthropic 官方对 `budget_tokens` 的常用区间:
1k~4k 属浅思考,4k~16k 属中等,16k 以上属深思考。

**必须复用 `clamp_effort` 的原因**:早期版本这里直接返回 `"medium"`,
一个 A 类模型目录里不存在的档位,造成两条 API 表面行为不一致。
共用函数是唯一能防止再次漂移的做法。

### 2.5 正确形态:按模型动态 clamp(当前实现的缺口)

当前 `SUPPORTED_EFFORTS` 是编译期常量,**这是已知缺口**。正确做法:

```rust
/// 从目录缓存里取目标模型支持的档位;取不到时退回最保守的公共子集。
fn supported_efforts_for(model: &str) -> Vec<&'static str> {
    let cat = catalogue_snapshot();                  // 见 §3.1
    match cat.reasoning_of(model) {
        Some(ReasoningDecl::Supported(list)) => list,
        Some(ReasoningDecl::SingleValue(_))  => vec!["high"],   // 无粗细可调
        Some(ReasoningDecl::None) | None     => vec![],         // 不透传
    }
}

/// 把标签落到该模型真实支持的档位之一。
fn clamp_onto(named: &str, supported: &[&'static str]) -> Option<&'static str> {
    if supported.is_empty() {
        return None;                       // 该模型不该收这个字段
    }
    if supported.contains(&named) {
        return Some(/* intern */);
    }
    // 按 low<medium<high<xhigh<max 的序,找最近的、方向偏上的档位
    let ladder = ["low", "medium", "high", "xhigh", "max"];
    let want = ladder.iter().position(|l| *l == named)?;
    supported
        .iter()
        .filter_map(|s| ladder.iter().position(|l| l == s).map(|p| (p, *s)))
        .filter(|(p, _)| *p >= want)
        .min_by_key(|(p, _)| *p)
        .or_else(|| supported.iter().filter_map(|s| {
            ladder.iter().position(|l| l == s).map(|p| (p, *s))
        }).max_by_key(|(p, _)| *p))
        .map(|(_, s)| s)
}
```

这样:
- A 类 `medium` → `high`(上取)
- B 类 `medium` → `medium`(原样保留,不浪费算力)
- C 类 `xhigh` → `high`(该模型最高就是 high)
- E 类任何值 → `high`(唯一档)
- G 类 → 不透传

---

## 3. 实现步骤

按依赖顺序,每步都可独立验证。

### Step 1 — 拉目录并缓存

```rust
#[derive(Clone, Debug)]
enum ReasoningDecl {
    Supported(Vec<&'static str>),   // supportedEfforts
    SingleValue(String),            // 单值 effort
    None,                           // 无 reasoning 对象
}
```

- 启动时拉一次;失败不阻塞启动(降级到公共子集 `["low","high"]`,最保守)。
- 缓存 10 分钟;`GET /admin/api/models?refresh=true` 可强制刷新。
- 目录里有 37 个模型、每个约 1KB,整体 < 50KB,常驻内存无压力。

**注意**:目录是**账号相关**的。换了账号可能拿到不同集合(实测三个账号返回一致,
但不能假定永久一致)。缓存 key 应包含账号 uid,或至少在换账号时失效。

### Step 2 — 建立"模型 → 档位集合"查询

```rust
fn reasoning_of(&self, model: &str) -> Option<ReasoningDecl>
```

模型别名要处理:`auto`、`fast-model`、`balanced-model`、`deep-model` 是
**路由器名字**,不是真实模型。对它们只能按目录里 `reasoning.effort` 的单值猜,
或者干脆透传不 clamp。

### Step 3 — 请求侧归一化

| 客户端输入 | 上游应收到 |
| --- | --- |
| `reasoning.effort = "high"` | 该模型支持的最近档位 |
| `reasoning` 字段缺失 | `"none"`(显式关闭) |
| `reasoning.effort = "auto"` / 拼错 | `"none"`(**建议改成 400**,见 §5.7) |
| `thinking.type = "disabled"` | `"none"` |
| `thinking.type = "enabled"` + budget | 分桶后归一 |
| 模型属于 G 类 | **不发** `reasoning_effort` |

关键实现细节:**无条件写入字段**,不要只在"有值时才写"。

```rust
upstream_body.insert(
    "reasoning_effort".into(),
    Value::String(requested_effort.to_string()),
);
```

理由:字段缺席的语义在上游是"服务端决定",而服务端决定会随版本变化。
显式写 `none` 才是确定性行为。

### Step 4 — 补输出预算

思考 token 与正文**共享** `max_tokens`。不补默认值时,思考型模型可能把预算
全花在 reasoning 上,正文返回空。

```rust
const DEFAULT_MAX_OUTPUT_TOKENS: i64 = 8192;

let max_output_tokens = match upstream_body.get("max_tokens").and_then(Value::as_i64) {
    Some(n) => n,
    None => {
        upstream_body.insert("max_tokens".into(), json!(DEFAULT_MAX_OUTPUT_TOKENS));
        DEFAULT_MAX_OUTPUT_TOKENS
    }
};
```

### Step 5 — 响应侧回显实际转发值

回显的是**转发值**,不是客户端传的值。这是让"途中被改写"可被发现的前提。

```rust
json!({ "effort": requested_effort, "summary": Value::Null })
```

把客户端传 `medium`、代理发 `high` 的情况暴露出来,而不是伪装成一致。

### Step 6 — 透出 reasoning token 用量

```rust
fn reasoning_tokens_of(usage: Option<&Value>) -> Value {
    usage
        .and_then(|u| u.get("completion_tokens_details"))
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64)
        .map(|n| json!(n))
        .unwrap_or(Value::Null)
}
```

两个协议都要各自映射:

| 协议 | 字段路径 |
| --- | --- |
| OpenAI / Responses | `usage.completion_tokens_details.reasoning_tokens` (上游) → `usage.output_tokens_details.reasoning_tokens` (对外) |
| Anthropic | 同上 → `usage.output_tokens_details.reasoning_tokens` |

流式路径下,Anthropic 侧要在 `message_delta` 事件里带,非流式在 message 尾部带。

**警告**:该字段对部分模型恒为 0(见 §5.2),不能当唯一判据。

### Step 7 — 截断状态映射

`finish_reason = length` 意味着思考或正文被截断,不能报 `completed`:

```rust
fn terminal_status(finish: Option<&str>) -> &'static str {
    match finish {
        Some("length") | Some("content_filter") => "incomplete",
        _ => "completed",
    }
}
```

### Step 8 — 测试

必须覆盖的断言:

```rust
#[test]
fn supported_efforts_pass_through_and_the_rest_clamp_up() {
    assert_eq!(clamp_effort("low"), "low");
    assert_eq!(clamp_effort("max"), "max");
    assert_eq!(clamp_effort("minimal"), "low");
    assert_eq!(clamp_effort("medium"), "high");
    assert_eq!(clamp_effort("xhigh"), "max");
    // 关键:任何已识别档位都不许塌成"关闭"
    for e in RESPONSES_REASONING_EFFORTS {
        assert_ne!(clamp_effort(e), REASONING_OFF);
    }
}

#[test]
fn disabled_thinking_states_the_off_level() {
    let body = json!({"thinking": {"type": "disabled"}});
    assert_eq!(reasoning_effort_from_thinking(body.as_object().unwrap()), Some("none"));
}

#[test]
fn thinking_budget_maps_onto_a_supported_level() {
    for (budget, want) in [(1024, "low"), (8000, "high"), (32000, "max")] {
        let body = json!({"thinking": {"type": "enabled", "budget_tokens": budget}});
        assert_eq!(
            reasoning_effort_from_thinking(body.as_object().unwrap()),
            Some(want)
        );
    }
}
```

---

## 4. 架构设计

### 4.1 分层

```
┌─────────────────────────────────────────────────────────┐
│ 协议适配层                                                │
│   responses.rs  reasoning.effort  ─┐                     │
│   anthropic.rs  thinking.budget   ─┤                     │
├────────────────────────────────────┼─────────────────────┤
│ 档位归一化层(唯一真相)             ▼                     │
│   clamp_onto(named, supported)                          │
│     ↑                                                    │
│   catalogue: model → ReasoningDecl  (内存缓存 10min)     │
├─────────────────────────────────────────────────────────┤
│ 上游转发层                                                │
│   无条件写 reasoning_effort + 补 max_tokens              │
├─────────────────────────────────────────────────────────┤
│ 观测层                                                    │
│   回显 effort / 透出 reasoning_tokens / incomplete 状态   │
└─────────────────────────────────────────────────────────┘
```

**核心原则:归一化只有一处实现。** 两条协议各自只负责"把自家输入翻译成
一个具名档位",之后走同一条路。早期 bug 就是 anthropic 侧自己算了一套。

### 4.2 数据流

```
客户端               网关                          上游
  │  effort=medium    │                              │
  ├──────────────────▶│ 查 catalogue[model]           │
  │                   │  → ["low","medium","high",    │
  │                   │     "xhigh","max"]            │
  │                   │ medium 在集合内 → 原样        │
  │                   ├── reasoning_effort=medium ───▶│
  │                   │                               │
  │                   │◀── usage.reasoning_tokens ────┤
  │◀─ effort=medium ──┤ 回显转发值,不美化            │
  │    reasoning_tokens=n                             │
```

### 4.3 缓存与失效

| 事件 | 动作 |
| --- | --- |
| 启动 | 拉一次;失败 → 用 `["low","high"]` 兜底,记 WARN |
| 每 10 分钟 | 后台刷新 |
| 账号切换 / 新增 | 立即失效 |
| `?refresh=true` | 同步刷新,失败不覆盖旧缓存 |
| 上游 5xx | 保留旧缓存,不降级到空 |

**不要**在请求路径上同步拉目录——目录接口 ~600ms,会把每个请求都拖慢。

### 4.4 降级策略

按"猜错的代价"排序:

1. **有缓存** → 按缓存 clamp(正常路径)
2. **无缓存但模型名已知** → 用该类别的公共子集
   - 三档命名(`glm-*`, `kimi-*`, `deepseek-*`) → `["low","high","max"]`
   - 未知 → `["low","high"]`(最保守,两档几乎总被支持)
3. **模型属于 G 类或无声明** → **不透传** `reasoning_effort`

宁可少一个参数,也不要发一个可能 400 的值。

---

## 5. 实际实施中会遇到的问题

以下全部来自实测,不是推测。原始数据见 §7。

### 5.1 `canDisableThinking` 不可信

目录声明 `canDisableThinking: false` 的模型,实测行为分两种:

| 模型 | 声明 | 传 `none` 实测 reasoning_tokens | 结论 |
| --- | --- | --- | --- |
| `space-bunny` | `false` | **47 / 47** | 真的关不掉 |
| `hy4free` | `false` | **0 / 0** | 其实能关 |
| `glm-5.3` | `false` | 1 | 能关 |

**所以不能用这个字段决定"要不要发 none"。** 发出去最坏情况是被忽略
(space-bunny),不发则一定关不掉。策略:**总是发**,并在文档里告诉用户
"部分模型无法真正关闭思考"。

### 5.2 `reasoning_tokens` 会漏报(最危险的一条)

`step-5-preview` 实测 16 个样本,**`reasoning_tokens` 全部为 0**,
但同批响应里 `delta.reasoning_content` 有 357~939 字符:

```
effort     http  rsn_chars  reasoning_tokens
None       200   556        0
none       200   435        0
low        200   636        0
medium     200   645        0
high       200   669        0
max        200   939        0
```

它多数时候确实在思考,只是没在 usage 里报账(复现时也见过个别样本
`rsn_chars=0`,说明该模型自身行为就不稳定,这更坐实了它不可作为判据基准)。

**推论:`reasoning_tokens > 0` 是"思考发生了"的充分条件,不是必要条件。**
判定"是否真的思考"需要两个信号取并集:

```python
thinking_happened = (reasoning_tokens > 0) or (len(reasoning_content) > 0)
```

这条如果不注意,会把正常工作的链路误判为"档位没生效",进而做出错误的"修复"。
本轮开发中确实因此走过一次弯路。

### 5.3 名义档位 ≠ 可区分档位

| 模型 | 声明档位 | 实测可区分 |
| --- | --- | --- |
| `deepseek-v4.1-flash` | low/high/max | **2 档**(low≈high 约 170,max 约 223) |
| `space-bunny` | low/medium/high/xhigh/max | **3 档**(~60 / ~195 / ~478) |
| `hy4free` | high | **1 档**(全噪声 400~1000) |

`deepseek-v4.1-flash` 的 `low`(中位 167)与 `high`(中位 170)在 4 次采样里
完全重叠,统计上不可分。也就是说**声明三档,实际两档**。

对用户的影响:不要承诺"5 档精细控制"。应如实说明"名义 N 档,实测 M 档"。

### 5.4 单档模型对任何值都无差别

`hy4free`(声明只有 `high`)传 `low` / `medium` / `high` / `max` 得到的
reasoning_tokens 在 387~1022 之间随机波动,**没有单调性**。

处理建议:对这类模型,把任何输入都归一为 `high`(唯一档),
不要假装实现了深浅调节。

### 5.5 单值 `effort` 声明的模型语义不明

16 个模型只有 `reasoning.effort` 单值(如 `deep-model` 声明 `medium`)。
实测 `deep-model` 单样本:

```
none=9  low=9  high=19  xhigh=12  minimal=113  medium=274  max=563
```

`medium` 和 `max` 明显更深,其余接近噪声。样本量不足以定论,
但可以确定:**这些模型会响应 `reasoning_effort`**,只是档位语义与 A 类不同。

建议:把它们当"单档/无粗细"处理,统一 `high`,并在文档标注"档位语义未明"。

### 5.6 两条协议的命名冲突

Responses 的 `medium` 与 `low/medium/high/max/xhigh` 中的 `medium` 同名,
但 Anthropic 侧中间桶原本也叫 `medium`,而 A 类模型不认这个名字。
这就是为什么 Anthropic 分桶后**必须**再走一次 `clamp_effort`。

另一个坑:`thinking.type` 缺失时默认值。代码里 `unwrap_or("enabled")`——
若客户端省略该字段,会被当成"要思考"。这与"省略即关闭"的直觉相反,
但符合 Anthropic 协议(省略 `thinking` 等于不启用,而 `type` 缺失属于畸形请求)。
需要保持一致并在测试里固定。

### 5.7 拼错的档位会静默变成"不思考"

```rust
RESPONSES_REASONING_EFFORTS.iter().find(|e| *e == lower)?   // 找不到 → None
// → forwarded_effort() → "none"
```

客户端写 `"hihg"` 不会报错,直接变成关闭思考。用户以为在深度思考,
实际没有。**建议改成 400**,让配置错误可见:

```rust
_ => return Err(ApiError::bad_request(format!(
    "unknown reasoning.effort: {raw:?}; expected one of {RESPONSES_REASONING_EFFORTS:?}"
))),
```

代价是更严格:如果将来上游新增档位名,旧代理会先报错。
取舍点在于"错误可见"vs"向前兼容"。

### 5.8 缓存与多账号

目录是账号维度的。三个账号实测返回一致,但换账号时仍应失效缓存——
否则 A 账号的档位表可能被用到 B 账号的模型上,而两者 `supportedEfforts` 未必相同。

### 5.9 思考吃光输出预算

`max_tokens` 是 reasoning + content 的**总和**。
`space-bunny` 在 `max` 档实测单次 `reasoning_tokens` 达到 791,
若客户端设 `max_tokens: 200`,正文必然为空且 `finish_reason=length`。

处理:补默认 8192(§3 Step 4),并把 `length` 映射成 `incomplete`(§3 Step 7),
让客户端能区分"说完了"和"被截断了"。

### 5.10 流式路径的时序陷阱

Anthropic 流式必须在 `message_delta` 里带 usage,而 usage 只有上游最后一帧才有。
实现上要**先聚合、后收尾**:读完所有 `delta`,拿到 `finish_reason` 和 `usage`,
再决定发 `response.completed` 还是 `response.incomplete`。
提前发 `message_stop` 会导致 usage 丢失。

---

## 6. 验证方法

### 6.1 不要用"字段传过去了"当验证

假阳性来源:
- 请求里带了 `reasoning_effort` → 只证明网关写了,不证明上游认;
- 响应里有 reasoning item / `reasoning_content` → 可能是模型自带的;
- 回显 `effort: "high"` → 只证明网关自己想发 high。

### 6.2 正确判据

```python
def thinking_tokens(usage) -> int:
    return (usage.get("completion_tokens_details") or {}).get("reasoning_tokens") or 0

def did_think(usage, reasoning_text: str) -> bool:
    # 两者取并集:部分模型 usage 恒 0(§5.2)
    return thinking_tokens(usage) > 0 or len(reasoning_text) > 0
```

### 6.3 端到端核对表

| 请求 | 期望 |
| --- | --- |
| 不传 `thinking` | 0 / 无思考块 |
| `type=disabled` | 0 / 无思考块(部分模型做不到,见 §5.1) |
| `budget=1024` | >0,浅 |
| `budget=8000` | >0,中 |
| `budget=32000` | >0,深 |
| `effort=medium` | 回显值 == 实际支持档位,不是 `none` |

每档至少 3~4 次采样,取中位数。单次采样不足以判断——`space-bunny` 同档位
两次相差 625 tokens。

### 6.4 回归脚本

`reasoning-tokens-check.py` 逐档扫描 `/v1/responses` 流式与非流式,
断言两条契约:

1. 请求的档位必须产生思考;档位缺席必须不产生;
2. 回显的 `effort` 必须等于实际转发值。

全失败时报 `INCONCLUSIVE` 并以非零码退出——**避免把"链路挂了"报告成"行为正确"**。

---

## 7. 附录:实测原始数据

以下数据全部由 `scripts/probe-upstream.py` 直连 `/v2/chat/completions` 采集,
**绕过代理**——这是把"上游没思考"和"我们的代理没记录"区分开的唯一办法。

```bash
python3 scripts/probe-upstream.py --model space-bunny --repeat 2
python3 scripts/probe-upstream.py --model hy4free --levels absent,none,low,high --repeat 3
```

每格写两个数字表示两次采样的值。`absent` = 不传 `reasoning_effort`。

### 7.1 `space-bunny`(声明 5 档,default=max,canDisable=false)

| effort | reasoning_tokens | rsn_chars |
| --- | --- | --- |
| (不传) | 50 / 38 | 257 / 200 |
| `none` | **47 / 47** | 298 / 267 |
| `minimal` | 151 / 19 | 840 / 106 |
| `low` | 73 / 63 | 407 / 343 |
| `medium` | 58 / 63 | 339 / 316 |
| `high` | 240 / 152 | 1001 / 583 |
| `xhigh` | 226 / 164 | 894 / 810 |
| `max` | 166 / 791 | 582 / 2868 |

观察:`none` 关不掉;`low`≈`medium`,`high`≈`xhigh`,`max` 最深。

### 7.2 `step-5-preview`(声明 low/medium/high,canDisable=false)

| effort | reasoning_tokens | rsn_chars |
| --- | --- | --- |
| (不传) | **0 / 0** | 556 / 665 |
| `none` | 0 / 0 | 435 / 556 |
| `low` | 0 / 0 | 636 / 357 |
| `medium` | 0 / 0 | 645 / 609 |
| `high` | 0 / 0 | 669 / 726 |
| `max` | 0 / 0 | 497 / 939 |

观察:usage 恒 0,**但确实在思考**。档位无法从 usage 区分。

### 7.3 `hy4free`(声明仅 high,canDisable=false)

| effort | reasoning_tokens |
| --- | --- |
| (不传) | 443 / 476 |
| `none` | **0 / 0** |
| `minimal` | 704 / 500 |
| `low` | 1022 / 617 |
| `medium` | 884 / 573 |
| `high` | 865 / 958 |
| `xhigh` | 634 / 387 |
| `max` | 771 / 813 |

观察:`none` 能关;其他档位无单调性,单档模型确实不可调。

### 7.4 `deepseek-v4.1-flash`(声明 low/high/max,canDisable=true)

经 `/v1/messages` 端到端,每档 4 次:

| 请求 | reasoning_tokens | 思考块 |
| --- | --- | --- |
| 不传 | 0 | 0/4 |
| `disabled` | 0 | 0/4 |
| `budget=1024` | 167 | 4/4 |
| `budget=8000` | 170 | 4/4 |
| `budget=32000` | 223 | 4/4 |

观察:开关完全可控;`low` 与 `high` 不可分,`max` 深 47%。

---

## 8. 变更清单(对照实现)

| 文件 | 内容 |
| --- | --- |
| `src/api/responses.rs` | `SUPPORTED_EFFORTS`、`clamp_effort`、`reasoning_effort`、`forwarded_effort`、`REASONING_OFF`、`DEFAULT_MAX_OUTPUT_TOKENS`、`terminal_status`、`incomplete_details`、`base_response` 回显 |
| `src/api/anthropic.rs` | `reasoning_effort_from_thinking`(分桶 + 复用 clamp)、`reasoning_tokens_of`、`message_delta` 带 usage |
| `scripts/dump-model-efforts.py` | 拉目录、按档位集合分组、导出原始 JSON |
| `scripts/probe-upstream.py` | 绕过代理直连上游,逐档采集 `reasoning_tokens` 与 `reasoning_content` 字符数 |

**已知缺口**:`SUPPORTED_EFFORTS` 仍是编译期常量,对 B 类(5 档)模型会过度升级。
修复路径见 §2.5。
