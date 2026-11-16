//! WB · auto —— 一个对外可见的合成模型名，按请求内容从**已可用**的模型里选一个转发。
//!
//! 本模块**无 IO、无状态、无 JS 前身**：只做「给一份 `usable_models()` 快照 + 一份请求体，
//! 选出该用哪个模型的全限定 id」。选中之后的链路（`prepare` → `backend` → `record`）一字不改，
//! 因此被选中模型的失败仍走既有判决（`orchestration.rs::record` 的 `validated.remove`）。
//!
//! 选择偏好由**速度权重**决定：探测成功时按耗时定权（[`probe_weight`]），真实请求过慢时扣分
//! （[`decayed_weight`]），选档取同档候选里权重最高者（[`select_weighted_with`]）。权重本身存在
//! `status.json` 的 `modelResults.<id>.weight`，本模块只提供纯函数，读写与「归零后重检」在编排层。
//!
//! 三条设计约束（详见 `docs/plans/2026-10-10-wb-auto-smart-router.md` §2）：
//! 1. 返回值必须是**全限定 id**（如 `opencode/glm-4.7-flash-free`），不能是面板用的 client id
//!    （`OC · 名称`）——记账键与实际模型必须是同一个值，否则失败判决打在假键上。
//! 2. 能力谓词一律按「缺键即 false」比较（`== Some(&json!(true))`），与
//!    `backend.rs::free_models_in` 归一出来的 `images` / `video` / `toolcall` 布尔值同形态。
//! 3. 请求要求的能力（图片 / 视频 / 工具）按**子集判定**过滤候选：`prepare` 的各道闸门
//!    （图片/视频 → 400 `unsupported_content`，chatOnly 带 tools → 400 `tools_not_supported`）
//!    都是**服务端本地拒绝**，选错档位会让「带截图 + 带工具定义」这种常见 agent 请求注定 400。
//!    图片与视频同走 `image_url` 通道（OpenAI 兼容层没有独立的视频 part），按 data URL 的
//!    mime 前缀分流（[`has_images`] / [`has_videos`]）。
//! 4. 带推理档位的请求再叠一层档位过滤（[`accepts_effort`]，与 `prepare` 的
//!    `unsupported_reasoning_effort` 判决同源）：DSH 里选中的档位随请求进来，自动路由若落到
//!    没有该 variant 的模型上，就是一个注定 400 的落点。

use crate::protocol::{random_uuid, DEFAULT_EFFORTS};
use crate::reasoning::reasoning_efforts;
use serde_json::{json, Value};

/// 对外暴露的唯一字面量：客户端把 `model` 配成它即可获得自动路由。
pub const AUTO_MODEL_ID: &str = "WB · auto";

/// 请求是否携带图片：任一 message 的 `content` 是数组且含 `type == "image_url"` 的 part，
/// 且该 part 的 `url` **不是**视频 data URL——OpenAI 兼容层没有独立的视频 part 形态，视频与图片
/// 同走 `image_url` 通道（`protocol.rs::prepare` 按 data URL 的 mime 前缀分流），不排除
/// `data:video/` 的话纯视频请求会被误当成图片档去选模型。`url` 缺失 / 非字符串仍按图片计：
/// 那种 part 在 `prepare` 里落进图片报错路径，选择器要与该判决一致。字符串 content 恒为 false
/// —— 与 `prepare` 的多模态解析同形态（`protocol.rs` 的 content 分支）。
pub fn has_images(messages: &[Value]) -> bool {
    messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts.iter().any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("image_url")
                        && !part
                            .get("image_url")
                            .and_then(|image| image.get("url"))
                            .and_then(Value::as_str)
                            .is_some_and(|url| url.starts_with("data:video/"))
                })
            })
    })
}

/// 请求是否携带视频：任一 `image_url` part 的 `url` 是以 `data:video/` 开头的字符串。
/// 只按前缀分流、不在这里校验容器格式——mime 是否受支持由 `prepare` 的
/// `parse_video_data_url` 判决，选档层只需要知道「这是视频请求」。
pub fn has_videos(messages: &[Value]) -> bool {
    messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts.iter().any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("image_url")
                        && part
                            .get("image_url")
                            .and_then(|image| image.get("url"))
                            .and_then(Value::as_str)
                            .is_some_and(|url| url.starts_with("data:video/"))
                })
            })
    })
}

/// 请求是否携带**非空** tools 数组。null / 缺失 / 空数组 / 非数组都是 false，
/// 与 `prepare` 把 `tools` 归一成 `[]` 后再判定的口径一致。
pub fn has_tools(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|items| !items.is_empty())
}

/// 能力位读取：缺失、null、非布尔真值都按 false（不做 truthy 宽松判定）。
fn flag(model: &Value, key: &str) -> bool {
    model.get(key) == Some(&json!(true))
}

/// 按请求内容判定单个候选模型是否合格：**子集判定**——请求要求的每一项能力（图片 / 视频 /
/// 工具），模型的对应能力位都必须为真（缺键即 false，与 [`flag`] 同源）。子集写法与旧的
/// `images`/`tools` 组合真值表逐字节等价（不要求的项不设门槛），只是把视频这一维并进来。
fn fits(model: &Value, images: bool, videos: bool, tools: bool) -> bool {
    if flag(model, "chatOnly") {
        // chatOnly 模型没有工具能力（`prepare` 会当场 400），只能服务不含 tools 的请求；
        // 其余能力位不参与判定（与旧实现一致，chatOnly 条目按该分支短路）。
        return !tools;
    }
    (!images || flag(model, "images"))
        && (!videos || flag(model, "video"))
        && (!tools || flag(model, "toolcall"))
}

/// 池里是否存在**能接工具调用请求**的候选（即 `fits(model, false, false, true)` 为真）。
///
/// 唯一的消费者是插件配置里那条 `WB · auto` 的 `supportsToolCall`（`sync.rs::auto_entry`）：
/// 全池都是 chatOnly 时把该字段写成 true，插件就会按工具形态发请求，而工具档没有候选 → 按 D3
/// 退回全池 → 落到 chatOnly 上被 `prepare` 当场 400，这条别名于是恒定失败。声明口径必须与
/// 选择器的档位谓词同源，否则「插件以为能给」和「核心选得出」会分叉。
pub fn any_tool_capable(models: &[Value]) -> bool {
    models.iter().any(|model| fits(model, false, false, true))
}

/// 池里是否存在**能接图片请求**的候选：声明口径与 `prepare` 的图片闸门同源——模型声明
/// `images == true` 就能接住（chatOnly 条目同样适用，`prepare` 的图片路径先于 chatOnly 分支）。
///
/// 唯一的消费者是插件配置里那条 `WB · auto` 的 `supportsImages`（`sync.rs::auto_entry`）。
/// 刻意**不走** [`fits`] 的 chatOnly 短路：路由的图片档放行 chatOnly 条目（旧真值表冻结的行为），
/// 但未声明 `images` 的模型随后必被 `prepare` 判 400 `unsupported_content`——把短路的 true
/// 声明出去就是「声明了却没有落点」，正是 [`any_tool_capable`] 文档警告的那类恒定失败入口。
/// 统计域由调用方决定（生产链传剔除付费后的免费池）。
pub fn any_image_capable(models: &[Value]) -> bool {
    models.iter().any(|model| flag(model, "images"))
}

/// 池里是否存在**能接视频请求**的候选：与 [`any_image_capable`] 同法，声明口径与 `prepare`
/// 的视频闸门同源（`video == true` 即能接住，chatOnly 条目同样适用）。
///
/// 唯一的消费者是插件配置里那条 `WB · auto` 的 `supportsVideo`（`sync.rs::auto_entry`）。
/// 统计域由调用方决定（生产链传剔除付费后的免费池）。
pub fn any_video_capable(models: &[Value]) -> bool {
    models.iter().any(|model| flag(model, "video"))
}

/// `WB · auto` 在插件配置里的推理档位声明：池内各推理模型 `reasoning_efforts` 键集的并集。
///
/// 形态与逐模型条目的 [`crate::reasoning::work_buddy_reasoning`] 同形（WorkBuddy 消费的是同一套键）：
/// - 池里没有 `reasoning == true` 的模型 → `{ supportsReasoning: false }`；
/// - 并集里没有任何非 `none` 档（全是固定推理 / 仅 `none` 的模型）→ 复用
///   [`crate::reasoning::work_buddy_reasoning`] 对「无可用档位」的形态：只声明会推理，
///   不写 `defaultEffort`，`supportedEfforts` 为空；
/// - 其余 → 完整块：`supportedEfforts` 按 [`crate::reasoning::EFFORT_LEVELS`] 顺序取并集（含 `none`），
///   `defaultEffort` 优先 `medium`、否则首个非 `none` 档，`canDisableThinking` = 并集含 `none`，
///   `onlyReasoning` 与逐模型 [`crate::reasoning::work_buddy_reasoning`] 同式（`!canDisableThinking`，
///   并集不含 `none` 即「只推理」）——同一能力集在 `models.json` 里不得有两种说法。
///
/// 统计域由调用方决定（生产链传剔除付费后的免费池）：付费模型独有的档位不进并集，
/// 否则插件会发出一个「选择器选不出落点」的档位请求——与 DSH 的 `auto_levels` 同一口径。
/// 并集里的每一档都至少有一个免费模型能接住（`candidates` 按请求档位过滤），因此声明即可达。
pub fn auto_reasoning(models: &[Value]) -> Value {
    // 与 dsh.rs::auto_levels 同构：按 EFFORT_LEVELS 的既定顺序过滤出并集，而不是先收集合再排序。
    let efforts_of: Vec<_> = models
        .iter()
        .filter(|model| flag(model, "reasoning"))
        .map(crate::reasoning::reasoning_efforts)
        .collect();
    if efforts_of.is_empty() {
        return json!({ "supportsReasoning": false });
    }
    let levels: Vec<&str> = crate::reasoning::EFFORT_LEVELS
        .into_iter()
        .filter(|level| {
            efforts_of
                .iter()
                .any(|efforts| efforts.contains_key(*level))
        })
        .collect();
    let enabled: Vec<&str> = levels.iter().copied().filter(|level| *level != "none").collect();
    if enabled.is_empty() {
        return json!({
            "supportsReasoning": true,
            "onlyReasoning": true,
            "reasoning": { "supportedEfforts": [], "canDisableThinking": false },
        });
    }
    let default_effort = if enabled.contains(&"medium") { "medium" } else { enabled[0] };
    json!({
        "supportsReasoning": true,
        "onlyReasoning": !levels.contains(&"none"),
        "reasoning": {
            "supportedEfforts": levels,
            "defaultEffort": default_effort,
            "canDisableThinking": levels.contains(&"none"),
        },
    })
}

/// 没有权重记录时的默认权重（满分）。旧版 `status.json` 的 `modelResults` 没有 `weight`，
/// 沿用上次结果时按满分处理——不能因为「旧数据缺这个字段」就把整池判成权重已耗尽。
pub const DEFAULT_WEIGHT: u32 = 100;

/// 一次真实请求整条耗时超过该值算「慢」，扣一次权重。
///
/// 口径是**整条请求**（含生成），长回答天然更久，所以阈值取 30s：只有明显偏慢的等待才扣分。
pub const SLOW_REQUEST_MS: i64 = 30_000;

/// 每次慢请求扣掉的权重。满分 100 → 连续 5 次慢请求归零；
/// 归零后由编排层的「全池权重为 0」判定触发一次全量重检。
pub const SLOW_PENALTY: u32 = 20;

/// 探测耗时 → 初始权重（分档，恒为正数）。
///
/// 探测是带工具调用的真实请求，其耗时近似「首包 + 一轮工具调用」，足以区分端点的快慢；
/// 权重必须 > 0，否则探测刚成功就会凑满「全池为 0」而立刻再次重检。
pub fn probe_weight(duration_ms: i64) -> u32 {
    match duration_ms {
        i64::MIN..=2_000 => 100,
        2_001..=5_000 => 80,
        5_001..=10_000 => 60,
        10_001..=20_000 => 40,
        20_001..=40_000 => 20,
        _ => 10,
    }
}

/// 模型当前权重；缺字段（旧状态、尚未探测）按 [`DEFAULT_WEIGHT`]。
pub fn weight_of(model: &Value) -> u32 {
    model
        .get("weight")
        .and_then(Value::as_u64)
        .map(|weight| u32::try_from(weight).unwrap_or(u32::MAX))
        .unwrap_or(DEFAULT_WEIGHT)
}

/// 慢请求扣权重，快请求不动；扣到 0 为止，不回负。
pub fn decayed_weight(current: u32, duration_ms: i64) -> u32 {
    if duration_ms >= SLOW_REQUEST_MS {
        current.saturating_sub(SLOW_PENALTY)
    } else {
        current
    }
}

/// 一次结果对权重的更新。**必须在状态锁内调用**（基于上一份权重计算），否则并发请求
/// 各自读旧值再写回会丢掉扣分。
///
/// - 探测成功：按本次耗时重新定权（手工「重新检测」也因此能重新分配权重）；
/// - 真实请求成功：慢请求扣分；
/// - 失败与其它来源：保持原权重（`None` 表示该模型还没有权重记录）。
pub fn next_weight(
    current: Option<u32>,
    ok: bool,
    source: &str,
    duration_ms: i64,
) -> Option<u32> {
    if !ok {
        return current;
    }
    match source {
        "probe" => Some(probe_weight(duration_ms)),
        "request" => Some(decayed_weight(current.unwrap_or(DEFAULT_WEIGHT), duration_ms)),
        _ => current,
    }
}

/// 池里所有权重都已归零 → 需要重新检测并重新分配权重。
///
/// 空池返回 false：没有可检的模型，也避免调用方把「池空」误当成触发条件。
pub fn all_weights_exhausted(models: &[Value]) -> bool {
    !models.is_empty() && models.iter().all(|model| weight_of(model) == 0)
}

/// `WB · auto` 的选择域：把付费白名单条目（`paid: true`）剔掉。
///
/// 付费模型只能由用户在客户端显式选中——自动路由与翻译模型候选都不得替用户产生费用。
/// 池里只剩付费模型时返回空集，调用方按既有「选不出不改写」处理。
pub fn free_pool(models: &[Value]) -> Vec<Value> {
    models
        .iter()
        .filter(|model| model.get("paid") != Some(&json!(true)))
        .cloned()
        .collect()
}

/// 请求携带的推理档位：`reasoning_effort` 优先，其次 OpenAI 的嵌套 `reasoning.effort`。
/// null 与非字符串一律视为未携带——与 `protocol::prepare` 的取读同形态（那边把「携带但非法」
/// 判成 400，这里只是不做档位过滤、把判决交回去）。
pub fn requested_effort(body: &Value) -> Option<&str> {
    body.get("reasoning_effort")
        .filter(|value| !value.is_null())
        .or_else(|| {
            body.get("reasoning")
                .and_then(|reasoning| reasoning.get("effort"))
                .filter(|value| !value.is_null())
        })
        .and_then(Value::as_str)
}

/// 模型能否接住该档位——镜像 `protocol::prepare` 的三条判决：必须声明 `reasoning`；
/// 有 variant 时只认 variant 覆盖的档位；`variants` 键数为 0 的固定推理模型认
/// [`DEFAULT_EFFORTS`]（WorkBuddy 对这类模型固定带 high 兜底，走的正是这条免除路径）。
/// 豁免的第三个条件必须连「没有任何 variants 键」一起判——`variants` 非空但全部被禁用 /
/// 缺合法 `reasoningEffort` 时 `reasoning_efforts` 也是空 map，而 `prepare` 对那种模型不给豁免
///（判 400 `unsupported_reasoning_effort`），路由放行就会把带档位的请求送进注定 400 的落点。
fn accepts_effort(model: &Value, effort: &str) -> bool {
    if !flag(model, "reasoning") {
        return false;
    }
    let efforts = reasoning_efforts(model);
    if efforts.contains_key(effort) {
        return true;
    }
    crate::json::key_count(model.get("variants")) == 0 && DEFAULT_EFFORTS.contains(&effort)
}

/// 按请求内容筛出候选集（能力档位与推理档位，见模块头注释第 3、4 条）。候选层为空时退回全池。
fn candidates<'a>(body: &Value, pool: &'a [Value]) -> Vec<&'a Value> {
    let messages: &[Value] = body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let images = has_images(messages);
    let videos = has_videos(messages);
    let tools = has_tools(body);
    let effort = requested_effort(body);

    let matching: Vec<&Value> = pool
        .iter()
        .filter(|model| fits(model, images, videos, tools))
        .filter(|model| effort.is_none_or(|name| accepts_effort(model, name)))
        .collect();
    // 退全池而非「报一个专用错误」：与用户手动选到能力不匹配的模型时的行为保持一致。
    if matching.is_empty() {
        pool.iter().collect()
    } else {
        matching
    }
}

/// 核心选择器（纯函数，选择器注入以便单测钉住「候选域是谁」）。
///
/// `pool` 必须是调用方**只取读一次**的 `usable_models()` 快照；`pick(len)` 返回候选集中的下标
/// （只被调用一次，故取 `FnOnce`，测试因此能在闭包里记下候选域大小），`None` 表示放弃本次路由
/// （交回 `prepare` 走既有 400）。候选层为空时退回全池，全池也为空则返回 `None` ——
/// 不做静默兜底、不新增错误码。
pub fn select_with(
    body: &Value,
    pool: &[Value],
    pick: impl FnOnce(usize) -> Option<usize>,
) -> Option<String> {
    let candidates = candidates(body, pool);
    let index = pick(candidates.len())?;
    candidates
        .get(index)?
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 在候选集里找出权重最高的下标；同权重时把 `pick(数量)` 交给调用方（生产入口用随机，
/// 单测用固定值），避免同权重模型被列表顺序钉死在某一个上。
fn highest_weight(
    candidates: &[&Value],
    pick: impl FnOnce(usize) -> Option<usize>,
) -> Option<usize> {
    let max = candidates.iter().map(|model| weight_of(model)).max()?;
    let top: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, model)| weight_of(model) == max)
        .map(|(index, _)| index)
        .collect();
    let slot = pick(top.len())?;
    top.get(slot).copied()
}

/// 按速度权重选择：只在**能力档位候选**里取权重最高的模型，同权重随机分散。
///
/// `pick(len)` 与 [`select_with`] 同语义：参数是同权重候选的数量。
pub fn select_weighted_with(
    body: &Value,
    pool: &[Value],
    pick: impl FnOnce(usize) -> Option<usize>,
) -> Option<String> {
    let candidates = candidates(body, pool);
    let index = highest_weight(&candidates, pick)?;
    candidates
        .get(index)?
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// 0..len 的随机下标：复用既有 `random_uuid()`（uuid v4，OS CSPRNG 驱动），取其前 4 个
/// hex 位（16 bit）对数量取模。候选规模 ~30 时模偏差 <0.05%，足够分散；
/// 不为此引入 `fastrand` / `rand`。
fn random_index(len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let hex = random_uuid().replace('-', "");
    let head = hex.get(..4)?;
    let value = u32::from_str_radix(head, 16).ok()?;
    Some((value % len as u32) as usize)
}

/// 生产入口：同档候选里取权重最高者；同权重时随机取一个（负载分散）。
pub fn auto_select(body: &Value, pool: &[Value]) -> Option<String> {
    select_weighted_with(body, pool, random_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// 池条目的最小形态：`usable_models()` 的字段远多于这些，路由只读 id 与三个能力位。
    fn entry(id: &str, images: bool, toolcall: bool, chat_only: bool) -> Value {
        json!({ "id": id, "images": images, "toolcall": toolcall, "chatOnly": chat_only })
    }

    fn pool() -> Vec<Value> {
        vec![
            entry("opencode/text-only", false, false, false),
            entry("opencode/vision", true, false, false),
            entry("opencode/agent", false, true, false),
            entry("opencode/all", true, true, false),
            entry("opencode/chat", false, false, true),
        ]
    }

    fn body(model: &str, content: Value, tools: Value) -> Value {
        json!({ "model": model, "messages": [{ "role": "user", "content": content }], "tools": tools })
    }

    /// 固定选择器：始终取第一个候选（顺序由池本身的顺序决定），便于断言候选域。
    fn first(len: usize) -> Option<usize> {
        (len > 0).then_some(0)
    }

    #[test]
    fn plain_text_considers_the_whole_pool_including_chat_only_models() {
        let got = select_with(&body(AUTO_MODEL_ID, json!("hi"), Value::Null), &pool(), first);
        assert_eq!(
            got.as_deref(),
            Some("opencode/text-only"),
            "纯文本的候选域必须是全池（含 chatOnly）"
        );
        // 候选长度即全池大小，说明没有任何能力过滤。
        let mut seen: Option<usize> = None;
        let len = select_with(&body(AUTO_MODEL_ID, json!("hi"), Value::Null), &pool(), |n| {
            seen = Some(n);
            Some(n - 1)
        });
        assert_eq!(seen, Some(5), "纯文本层不得过滤掉任何模型");
        assert_eq!(len.as_deref(), Some("opencode/chat"), "末位候选是 chatOnly 模型");
    }

    #[test]
    fn image_requests_only_select_models_declaring_image_input() {
        let images = json!([
            { "type": "text", "text": "看看这张图" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
        ]);
        let got = select_with(&body(AUTO_MODEL_ID, images, Value::Null), &pool(), first);
        assert_eq!(got.as_deref(), Some("opencode/vision"), "首个合格候选");
        // content 是数组但全是 text part：没有图片，候选域仍是全池（末位即 chatOnly）。
        let plain = json!([{ "type": "text", "text": "只有文字" }]);
        let got = select_with(&body(AUTO_MODEL_ID, plain, Value::Null), &pool(), |n| Some(n - 1));
        assert_eq!(got.as_deref(), Some("opencode/chat"));
    }

    #[test]
    fn tool_requests_exclude_chat_only_and_image_only_models() {
        let tools = json!([{ "type": "function", "function": { "name": "read_file" } }]);
        let got = select_with(&body(AUTO_MODEL_ID, json!("用工具"), tools.clone()), &pool(), first);
        assert_eq!(got.as_deref(), Some("opencode/agent"));
        let mut domain: Vec<usize> = Vec::new();
        let _ = select_with(&body(AUTO_MODEL_ID, json!("用工具"), tools), &pool(), |n| {
            domain.push(n);
            Some(0)
        });
        assert_eq!(domain, vec![2], "工具层候选只有 toolcall 且非 chatOnly 的两个");
    }

    #[test]
    fn image_plus_tools_requires_both_capabilities() {
        let content = json!([
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
        ]);
        let tools = json!([{ "type": "function", "function": { "name": "run" } }]);
        let mut domain = Vec::new();
        let got = select_with(&body(AUTO_MODEL_ID, content, tools), &pool(), |n| {
            domain.push(n);
            Some(0)
        });
        assert_eq!(got.as_deref(), Some("opencode/all"), "混合请求只能落在双能力模型上");
        assert_eq!(domain, vec![1], "候选集恰为唯一的双能力模型");
    }

    #[test]
    fn empty_image_tier_falls_back_to_the_whole_pool() {
        // 池里没有 images 模型时，图片请求退全池（该请求随后要么被 prepare 本地 400、要么成功）。
        let only_text = vec![entry("opencode/text-only", false, false, false)];
        let content = json!([{ "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }]);
        let got = select_with(&body(AUTO_MODEL_ID, content, Value::Null), &only_text, first);
        assert_eq!(got.as_deref(), Some("opencode/text-only"), "候选空时退回全池而不是报新错");
    }

    #[test]
    fn empty_tool_tier_falls_back_to_the_whole_pool() {
        let only_text = vec![entry("opencode/text-only", false, false, false)];
        let tools = json!([{ "type": "function", "function": { "name": "run" } }]);
        let got = select_with(&body(AUTO_MODEL_ID, json!("hi"), tools), &only_text, first);
        assert_eq!(got.as_deref(), Some("opencode/text-only"));
    }

    #[test]
    fn an_empty_pool_selects_nothing() {
        let got = select_with(&body(AUTO_MODEL_ID, json!("hi"), Value::Null), &[], |_| Some(0));
        assert_eq!(got, None, "池空必须交回 prepare 报既有 400，绝不凭空造键");
    }

    #[test]
    fn a_single_candidate_still_goes_through_the_picker_with_len_one() {
        let dual = vec![entry("opencode/all", true, true, false)];
        let content = json!([{ "type": "image_url", "image_url": { "url": "data:image/gif;base64,AAAA" } }]);
        let tools = json!([{ "type": "function", "function": { "name": "run" } }]);
        let mut seen = None;
        let got = select_with(&body(AUTO_MODEL_ID, content, tools), &dual, |n| {
            seen = Some(n);
            Some(0)
        });
        assert_eq!(seen, Some(1), "唯一候选也要走选择器（len=1），与多候选同一条路径");
        assert_eq!(got.as_deref(), Some("opencode/all"));
    }

    /// 带 variant 的推理模型：`levels` 即它真正能服务的档位。
    fn reasoning_entry(id: &str, levels: &[&str]) -> Value {
        let variants = levels
            .iter()
            .map(|level| (level.to_string(), json!({ "reasoningEffort": level })))
            .collect::<serde_json::Map<_, _>>();
        json!({
            "id": id,
            "images": false,
            "toolcall": false,
            "chatOnly": false,
            "reasoning": true,
            "variants": variants,
        })
    }

    fn effort_pool() -> Vec<Value> {
        vec![
            entry("opencode/plain", false, false, false),
            reasoning_entry("opencode/low", &["low"]),
            reasoning_entry("opencode/high", &["high"]),
        ]
    }

    #[test]
    fn a_requested_effort_only_considers_models_that_can_serve_it() {
        let mut request = body(AUTO_MODEL_ID, json!("hi"), Value::Null);
        request["reasoning_effort"] = json!("high");
        assert_eq!(
            select_with(&request, &effort_pool(), first).as_deref(),
            Some("opencode/high"),
            "档位过滤后只剩能服务 high 的模型，普通模型与别的档位都不得入选"
        );
        request["reasoning_effort"] = json!("low");
        assert_eq!(select_with(&request, &effort_pool(), first).as_deref(), Some("opencode/low"));
    }

    #[test]
    fn nested_reasoning_effort_is_read_like_the_top_level_field() {
        assert_eq!(requested_effort(&json!({ "reasoning_effort": "high" })), Some("high"));
        assert_eq!(requested_effort(&json!({ "reasoning": { "effort": "low" } })), Some("low"));
        assert_eq!(requested_effort(&json!({ "reasoning_effort": null })), None);
        assert_eq!(requested_effort(&json!({ "reasoning": { "effort": null } })), None);
        assert_eq!(requested_effort(&json!({ "reasoning_effort": 3 })), None, "非字符串不参与过滤");

        let mut request = body(AUTO_MODEL_ID, json!("hi"), Value::Null);
        request["reasoning"] = json!({ "effort": "low" });
        assert_eq!(select_with(&request, &effort_pool(), first).as_deref(), Some("opencode/low"));
    }

    #[test]
    fn an_effort_no_model_serves_falls_back_to_the_whole_pool() {
        let mut request = body(AUTO_MODEL_ID, json!("hi"), Value::Null);
        request["reasoning_effort"] = json!("xhigh");
        let mut domain = None;
        let _ = select_with(&request, &effort_pool(), |n| {
            domain = Some(n);
            Some(0)
        });
        assert_eq!(domain, Some(3), "档位层为空时退全池，判决交回 prepare（与能力层同一条规矩）");
    }

    #[test]
    fn fixed_reasoning_models_serve_the_default_efforts_only() {
        let fixed = json!({
            "id": "opencode/fixed",
            "images": false,
            "toolcall": false,
            "chatOnly": false,
            "reasoning": true,
            "variants": {},
        });
        let pool = vec![entry("opencode/plain", false, false, false), fixed];
        let mut request = body(AUTO_MODEL_ID, json!("hi"), Value::Null);
        request["reasoning_effort"] = json!("high");
        assert_eq!(
            select_with(&request, &pool, first).as_deref(),
            Some("opencode/fixed"),
            "固定推理模型在 prepare 里认 DEFAULT_EFFORTS，路由必须让它入选"
        );
        request["reasoning_effort"] = json!("none");
        let mut domain = None;
        let _ = select_with(&request, &pool, |n| {
            domain = Some(n);
            Some(0)
        });
        assert_eq!(domain, Some(2), "none 不在 DEFAULT_EFFORTS 里：固定推理模型接不住，退全池");
    }

    #[test]
    fn effort_and_capability_tiers_intersect() {
        let pool = vec![
            json!({
                "id": "opencode/tools-low",
                "images": false,
                "toolcall": true,
                "chatOnly": false,
                "reasoning": true,
                "variants": { "low": { "reasoningEffort": "low" } },
            }),
            json!({
                "id": "opencode/tools-high",
                "images": false,
                "toolcall": true,
                "chatOnly": false,
                "reasoning": true,
                "variants": { "high": { "reasoningEffort": "high" } },
            }),
        ];
        let mut request = body(
            AUTO_MODEL_ID,
            json!("用工具"),
            json!([{ "type": "function", "function": { "name": "run" } }]),
        );
        request["reasoning_effort"] = json!("high");
        assert_eq!(
            select_with(&request, &pool, first).as_deref(),
            Some("opencode/tools-high"),
            "能力层与档位层必须同时满足"
        );
    }

    #[test]
    fn has_images_only_reads_array_content_with_image_parts() {
        assert!(has_images(&[json!({ "content": [{ "type": "image_url" }] })]));
        assert!(
            !has_images(&[json!({ "content": "图呢？" })]),
            "字符串 content 恒为无图片（与 prepare 的解析同形态）"
        );
        assert!(!has_images(&[json!({ "content": [] })]), "空 part 数组不算图片请求");
        assert!(
            !has_images(&[json!({ "content": [{ "type": "text", "text": "x" }] })]),
            "非 image_url 的 part 不得被当成图片"
        );
        assert!(
            !has_images(&[json!({ "content": null })]),
            "content 为 null 时 prepare 会归一成空串，同样不该被当成图片"
        );
        assert!(
            has_images(&[
                json!({ "content": [{ "type": "text", "text": "x" }] }),
                json!({ "content": [{ "type": "image_url", "image_url": { "url": "data:image/jpeg;base64,/9j" } }] }),
            ]),
            "任一条 message 带图片即算图片请求"
        );
    }

    #[test]
    fn has_tools_reads_only_nonempty_arrays() {
        assert!(has_tools(&json!({ "tools": [{ "type": "function" }] })));
        for (label, body) in [
            ("null", json!({ "tools": Value::Null })),
            ("缺失", json!({ "model": "x" })),
            ("空数组", json!({ "tools": [] })),
            ("非数组", json!({ "tools": { "name": "run" } })),
            ("字符串", json!({ "tools": "run" })),
        ] {
            assert!(!has_tools(&body), "{label} 必须按无工具处理");
        }
    }

    #[test]
    fn the_selected_id_is_the_fully_qualified_model_id() {
        // 记账键（validated / modelResults / usage）与响应回显都取这个值，client id 会让失败判决打在假键上。
        let got = select_with(&body(AUTO_MODEL_ID, json!("hi"), Value::Null), &pool(), |_| Some(3));
        assert_eq!(got.as_deref(), Some("opencode/all"));
        assert_ne!(got.as_deref(), Some("OC · All"));
    }

    #[test]
    fn picker_returning_none_abandons_the_route() {
        let got = select_with(&body(AUTO_MODEL_ID, json!("hi"), Value::Null), &pool(), |_| None);
        assert_eq!(got, None, "选择器放弃时不得改写 body，交给 prepare 的既有 400");
    }

    #[test]
    fn auto_select_always_lands_inside_the_pool_and_never_invents_wb_auto() {
        let body = body(AUTO_MODEL_ID, json!("hi"), Value::Null);
        let ids: Vec<String> = pool().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect();
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..200 {
            let got = auto_select(&body, &pool()).expect("池非空必有选择");
            assert!(ids.contains(&got), "选中的必须是池内 id：{got}");
            assert_ne!(got, AUTO_MODEL_ID, "绝不可能把自己路由给自己");
            distinct.insert(got);
        }
        assert!(distinct.len() > 1, "200 次选择必须分散到多个模型，否则是退化成固定选第一个");
    }

    #[test]
    fn auto_select_on_an_empty_pool_selects_nothing() {
        assert_eq!(auto_select(&body(AUTO_MODEL_ID, json!("hi"), Value::Null), &[]), None);
    }

    #[test]
    fn chat_only_models_stay_reachable_for_plain_text_but_never_for_tools() {
        let chat = vec![entry("opencode/chat", false, false, true)];
        let text = select_with(&body(AUTO_MODEL_ID, json!("hi"), Value::Null), &chat, first);
        assert_eq!(text.as_deref(), Some("opencode/chat"), "纯文本时 chatOnly 合法");
        let tools = json!([{ "type": "function", "function": { "name": "run" } }]);
        let forced = select_with(&body(AUTO_MODEL_ID, json!("hi"), tools), &chat, first);
        assert_eq!(
            forced.as_deref(),
            Some("opencode/chat"),
            "池里只有 chatOnly 时按「候选空→退全池」处理，随后的 400 由 prepare 给出（决策点 D3）"
        );
    }

    #[test]
    fn missing_capability_keys_are_read_as_false() {
        // 真实池条目一定有 images/toolcall（free_models_in 归一成布尔），这里钉住「缺键即 false」。
        let bare = vec![json!({ "id": "opencode/bare" }), entry("opencode/vision", true, false, false)];
        let content = json!([{ "type": "image_url", "image_url": { "url": "data:image/webp;base64,AAAA" } }]);
        let got = select_with(&body(AUTO_MODEL_ID, content, Value::Null), &bare, first);
        assert_eq!(got.as_deref(), Some("opencode/vision"));
    }

    #[test]
    fn selected_ids_survive_a_model_without_an_id_entry() {
        // 池里混进无 id 的条目时不得 panic，也绝不返回空串当模型 id。
        let mixed = vec![json!({ "name": "no id" }), entry("opencode/ok", false, false, false)];
        let got = select_with(&body(AUTO_MODEL_ID, json!("hi"), Value::Null), &mixed, |_| Some(0));
        assert_eq!(got, None, "选中条目没有 id 时必须放弃路由，而不是交出一个空键");
        let got = select_with(&body(AUTO_MODEL_ID, json!("hi"), Value::Null), &mixed, |_| Some(1));
        assert_eq!(got.as_deref(), Some("opencode/ok"));
    }

    #[test]
    fn auto_model_id_is_the_only_literal_the_surface_uses() {
        assert_eq!(AUTO_MODEL_ID, "WB · auto");
        assert!(!pool().iter().any(|m| m["id"] == json!(AUTO_MODEL_ID)));
    }

    // ------------------------------------------------------------------
    // 速度权重
    // ------------------------------------------------------------------

    /// 权重条目：`usable_models()` 里真实条目一定带能力位，这里同时带上 `weight`。
    fn weighted(id: &str, weight: u64, images: bool, toolcall: bool, chat_only: bool) -> Value {
        json!({
            "id": id,
            "images": images,
            "toolcall": toolcall,
            "chatOnly": chat_only,
            "weight": weight,
        })
    }

    /// 探测耗时是**初始权重**的唯一来源：越快越高，且每个成功的探测都必须留下正权重，
    /// 否则探测刚结束就会立刻凑满「全池为 0」再次触发重检。
    #[test]
    fn probe_speed_maps_to_a_positive_weight() {
        assert_eq!(probe_weight(500), 100);
        assert_eq!(probe_weight(2_000), 100);
        assert_eq!(probe_weight(2_001), 80);
        assert_eq!(probe_weight(5_000), 80);
        assert_eq!(probe_weight(10_000), 60);
        assert_eq!(probe_weight(20_000), 40);
        assert_eq!(probe_weight(40_000), 20);
        assert_eq!(probe_weight(40_001), 10);
        assert!(probe_weight(60_000) > 0, "探测成功后的权重必须为正");
    }

    /// 只有「真实请求成功且整条耗时 ≥ 30s」才扣分；快请求、失败、探测路径都不走扣分。
    #[test]
    fn weights_decay_only_on_slow_successful_requests() {
        assert_eq!(decayed_weight(100, SLOW_REQUEST_MS - 1), 100, "未过阈值不扣分");
        assert_eq!(decayed_weight(100, SLOW_REQUEST_MS), 80);
        assert_eq!(decayed_weight(20, SLOW_REQUEST_MS), 0);
        assert_eq!(decayed_weight(10, SLOW_REQUEST_MS), 0, "扣到 0 为止，不回负");

        assert_eq!(next_weight(Some(100), true, "request", 5_000), Some(100));
        assert_eq!(next_weight(Some(100), true, "request", 45_000), Some(80));
        assert_eq!(next_weight(None, true, "request", 45_000), Some(80), "旧数据缺权重按满分起步");
        assert_eq!(next_weight(Some(80), false, "request", 45_000), Some(80), "失败不改变权重");
        assert_eq!(next_weight(None, false, "request", 45_000), None, "没有历史权重就不凭空造");
    }

    /// 探测成功按本次耗时**重新定权**（即使上一份权重更高也会被覆盖），这是「重新分配权重」。
    #[test]
    fn a_successful_probe_reassigns_the_weight_from_its_own_latency() {
        assert_eq!(next_weight(Some(20), true, "probe", 1_000), Some(100));
        assert_eq!(next_weight(Some(100), true, "probe", 15_000), Some(40));
        assert_eq!(next_weight(None, true, "probe", 12_000), Some(40));
        // 探测失败保持原值：模型这次没通过，权重不参与选择，等下次通过再重定。
        assert_eq!(next_weight(Some(60), false, "probe", 1_000), Some(60));
    }

    /// 「全池为 0」只在池非空时成立；缺 `weight` 的旧条目按满分处理，不能把旧状态误判成耗尽。
    #[test]
    fn weight_exhaustion_needs_a_non_empty_pool_and_explicit_zeros() {
        assert!(!all_weights_exhausted(&[]), "空池不是「权重耗尽」");
        assert!(!all_weights_exhausted(&pool()), "缺 weight 的旧数据按满分处理");
        assert!(!all_weights_exhausted(&[
            weighted("opencode/a", 0, false, false, false),
            weighted("opencode/b", 20, false, false, false),
        ]));
        assert!(all_weights_exhausted(&[
            weighted("opencode/a", 0, false, false, false),
            weighted("opencode/b", 0, false, false, false),
        ]));
    }

    /// 默认选择必须落在**权重最高**的模型上，而不是随机分散到全池。
    #[test]
    fn auto_select_always_takes_the_highest_weight_candidate() {
        let pool = vec![
            weighted("opencode/slow", 20, false, false, false),
            weighted("opencode/fast", 100, false, false, false),
            weighted("opencode/mid", 60, false, false, false),
        ];
        let body = body(AUTO_MODEL_ID, json!("hi"), Value::Null);
        for _ in 0..50 {
            assert_eq!(auto_select(&body, &pool).as_deref(), Some("opencode/fast"));
        }
    }

    /// 同权重只在「并列最高」的那几个之间随机——低权重模型不该被抽中。
    #[test]
    fn ties_only_spread_across_the_highest_weight_models() {
        let pool = vec![
            weighted("opencode/a", 100, false, false, false),
            weighted("opencode/b", 100, false, false, false),
            weighted("opencode/low", 30, false, false, false),
        ];
        let body = body(AUTO_MODEL_ID, json!("hi"), Value::Null);
        let mut distinct = std::collections::HashSet::new();
        for _ in 0..200 {
            let got = auto_select(&body, &pool).expect("池非空必有选择");
            assert_ne!(got, "opencode/low", "低权重模型不得因随机性被选中");
            distinct.insert(got);
        }
        assert_eq!(distinct.len(), 2, "并列最高权重之间应当分散");
    }

    /// 权重只在能力档位**内部**比较：图片请求不得因为纯文本模型权重更高就选它。
    #[test]
    fn weighting_never_escapes_the_capability_tier() {
        let pool = vec![
            weighted("opencode/text", 100, false, false, false),
            weighted("opencode/vision", 10, true, false, false),
        ];
        let content = json!([
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
        ]);
        let got = select_weighted_with(&body(AUTO_MODEL_ID, content, Value::Null), &pool, first);
        assert_eq!(got.as_deref(), Some("opencode/vision"));

        let tools = json!([{ "type": "function", "function": { "name": "run" } }]);
        let pool = vec![
            weighted("opencode/chat", 100, false, false, true),
            weighted("opencode/agent", 10, false, true, false),
        ];
        let got = select_weighted_with(&body(AUTO_MODEL_ID, json!("hi"), tools), &pool, first);
        assert_eq!(got.as_deref(), Some("opencode/agent"), "工具档不得落到 chatOnly 上");
    }

    /// 选择器放弃时仍返回 None（交回 prepare 的既有 400），不因为有权重就兜底。
    #[test]
    fn a_weighted_picker_returning_none_abandons_the_route() {
        let pool = vec![weighted("opencode/a", 100, false, false, false)];
        let got = select_weighted_with(&body(AUTO_MODEL_ID, json!("hi"), Value::Null), &pool, |_| None);
        assert_eq!(got, None);
    }

    // ------------------------------------------------------------------
    // 视频档位与别名能力声明（2026-11-17：视频端到端 + 免费池如实声明）
    // ------------------------------------------------------------------

    /// 带视频能力位的池条目：`video` 与 `images` / `toolcall` 同形态（缺键即 false）。
    fn video_entry(id: &str, video: bool, images: bool, toolcall: bool, chat_only: bool) -> Value {
        json!({
            "id": id,
            "video": video,
            "images": images,
            "toolcall": toolcall,
            "chatOnly": chat_only,
        })
    }

    /// 视频走 `image_url` 通道进来，按 data URL 前缀分流：只有 `data:video/` 前缀算视频，
    /// 图片 URL、https URL、缺失 url 与非 image_url part 都不算。
    #[test]
    fn has_videos_reads_only_video_data_urls_in_image_url_parts() {
        assert!(has_videos(&[json!({
            "content": [{ "type": "image_url", "image_url": { "url": "data:video/mp4;base64,AAAA" } }]
        })]));
        assert!(
            !has_videos(&[json!({
                "content": [{ "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }]
            })]),
            "图片 data URL 不是视频"
        );
        assert!(
            !has_videos(&[json!({
                "content": [{ "type": "image_url", "image_url": { "url": "https://example.com/a.mp4" } }]
            })]),
            "非 data URL 的引用不得当成内联视频"
        );
        assert!(!has_videos(&[json!({ "content": [{ "type": "image_url" }] })]), "缺失 url 不算视频");
        assert!(!has_videos(&[json!({ "content": [{ "type": "text", "text": "x" }] })]));
        assert!(!has_videos(&[json!({ "content": "视频呢？" })]), "字符串 content 恒为无视频");

        // 视频与图片混在一个请求里：两个谓词同时为真。
        let mixed = [json!({ "content": [
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
            { "type": "image_url", "image_url": { "url": "data:video/webm;base64,AAAA" } },
        ] })];
        assert!(has_videos(&mixed));
        assert!(has_images(&mixed));
    }

    /// 纯视频请求不得被当成图片请求选模型；url 缺失 / 非字符串仍按图片计（落 prepare 的图片报错路径）。
    #[test]
    fn has_images_excludes_video_data_urls_but_keeps_missing_urls() {
        let video = json!([{ "type": "image_url", "image_url": { "url": "data:video/mp4;base64,AAAA" } }]);
        assert!(!has_images(&[json!({ "content": video })]), "纯视频请求不算图片请求");
        assert!(
            has_images(&[json!({ "content": [{ "type": "image_url" }] })]),
            "url 缺失仍按图片计（与 prepare 的图片报错路径一致）"
        );
        assert!(
            has_images(&[json!({
                "content": [{ "type": "image_url", "image_url": { "url": 42 } }]
            })]),
            "url 非字符串仍按图片计"
        );
    }

    /// 视频档路由：视频请求只落在声明了 video 的模型上；视频+tools、视频+images 的组合
    /// 请求要求候选同时声明两个能力位（子集判定）。
    #[test]
    fn video_requests_select_models_that_declare_video_input() {
        let pool = vec![
            video_entry("opencode/video", true, false, false, false),
            video_entry("opencode/video-tools", true, false, true, false),
            video_entry("opencode/video-images", true, true, false, false),
            video_entry("opencode/vision", false, true, false, false),
        ];
        let video_part = json!([{ "type": "image_url", "image_url": { "url": "data:video/mp4;base64,AAAA" } }]);
        let got = select_with(&body(AUTO_MODEL_ID, video_part.clone(), Value::Null), &pool, first);
        assert_eq!(got.as_deref(), Some("opencode/video"), "纯视频请求只落 video 模型");

        let tools = json!([{ "type": "function", "function": { "name": "run" } }]);
        let got = select_with(&body(AUTO_MODEL_ID, video_part.clone(), tools.clone()), &pool, first);
        assert_eq!(
            got.as_deref(),
            Some("opencode/video-tools"),
            "视频+工具的候选必须同时声明两个能力位"
        );

        let image_and_video = json!([
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
            { "type": "image_url", "image_url": { "url": "data:video/mp4;base64,AAAA" } },
        ]);
        let got = select_with(&body(AUTO_MODEL_ID, image_and_video, Value::Null), &pool, first);
        assert_eq!(
            got.as_deref(),
            Some("opencode/video-images"),
            "图片+视频的组合候选必须同时声明两个能力位"
        );

        // 与其它档同一规矩：视频档候选为空时退全池，判决交回 prepare。
        let only_images = vec![video_entry("opencode/vision", false, true, false, false)];
        let got = select_with(&body(AUTO_MODEL_ID, video_part, Value::Null), &only_images, first);
        assert_eq!(got.as_deref(), Some("opencode/vision"), "视频档无候选时退回全池而不是报新错");
    }

    /// 生产链把 `free_pool()` 的结果交给 any_*_capable：付费条目再全能也不为别名点亮任何声明。
    #[test]
    fn a_paid_only_pool_reports_no_capabilities_for_the_alias() {
        let pool = vec![json!({
            "id": "vendor/paid",
            "images": true,
            "video": true,
            "toolcall": true,
            "chatOnly": false,
            "paid": true,
        })];
        let free = free_pool(&pool);
        assert!(free.is_empty(), "付费-only 的池剔除后为空");
        assert!(!any_tool_capable(&free));
        assert!(!any_image_capable(&free));
        assert!(!any_video_capable(&free));
        // 对照组：同一条目转为免费后三个能力位如实点亮。
        let turned_free = vec![json!({
            "id": "vendor/paid", "images": true, "video": true, "toolcall": true, "chatOnly": false,
        })];
        assert!(any_tool_capable(&turned_free));
        assert!(any_image_capable(&turned_free));
        assert!(any_video_capable(&turned_free));
    }

    /// 推理档位并集：按 EFFORT_LEVELS 顺序（none 在首位）、medium 是首选默认档、含 none 即可关思考。
    #[test]
    fn auto_reasoning_unions_the_pool_efforts_in_canonical_order() {
        let pool = vec![
            entry("opencode/plain", false, false, false),
            reasoning_entry("opencode/a", &["high"]),
            reasoning_entry("opencode/b", &["none", "low"]),
            reasoning_entry("opencode/c", &["medium", "max"]),
        ];
        assert_eq!(
            auto_reasoning(&pool),
            json!({
                "supportsReasoning": true,
                "onlyReasoning": false,
                "reasoning": {
                    "supportedEfforts": ["none", "low", "medium", "high", "max"],
                    "defaultEffort": "medium",
                    "canDisableThinking": true,
                },
            }),
            "并集按 EFFORT_LEVELS 顺序排列，medium 是首选默认档"
        );

        // 并集里没有 medium 时取首个非 none 档。
        let no_medium = vec![reasoning_entry("opencode/a", &["low", "none"])];
        let declared = auto_reasoning(&no_medium);
        assert_eq!(declared["reasoning"]["supportedEfforts"], json!(["none", "low"]));
        assert_eq!(declared["reasoning"]["defaultEffort"], json!("low"));
        assert_eq!(declared["reasoning"]["canDisableThinking"], json!(true));
    }

    /// 池里没有推理模型（含空池）→ 只声明 `supportsReasoning: false`。
    #[test]
    fn auto_reasoning_without_any_reasoning_model_stays_flat() {
        assert_eq!(
            auto_reasoning(&[entry("opencode/plain", false, false, false)]),
            json!({ "supportsReasoning": false })
        );
        assert_eq!(auto_reasoning(&[]), json!({ "supportsReasoning": false }), "空池（付费-only 发布集）同样没有推理档位");
    }

    /// 并集里没有任何非 none 档（固定推理 / 仅 none 的模型）→ 复用 work_buddy_reasoning
    /// 的「只推理」形态，不写 defaultEffort。
    #[test]
    fn auto_reasoning_without_usable_levels_declares_only_reasoning() {
        let pool = vec![
            json!({ "id": "opencode/fixed", "reasoning": true, "variants": {} }),
            reasoning_entry("opencode/off", &["none"]),
            entry("opencode/plain", false, false, false),
        ];
        assert_eq!(
            auto_reasoning(&pool),
            json!({
                "supportsReasoning": true,
                "onlyReasoning": true,
                "reasoning": { "supportedEfforts": [], "canDisableThinking": false },
            })
        );
    }

    /// 生产链传的是 `free_pool()` 后的免费池：付费模型独有的档位不进并集
    /// （口径镜像 dsh.rs::reasoning_levels_are_declared_per_model_and_unioned_on_auto）。
    #[test]
    fn auto_reasoning_never_lets_paid_models_donate_levels() {
        let pool = vec![
            entry("opencode/plain", false, false, false),
            reasoning_entry("opencode/free-low", &["low"]),
            {
                let mut paid = reasoning_entry("opencode/paid-max", &["max"]);
                paid["paid"] = json!(true);
                paid
            },
        ];
        let declared = auto_reasoning(&free_pool(&pool));
        assert_eq!(
            declared["reasoning"]["supportedEfforts"],
            json!(["low"]),
            "max 只在付费模型上，不得进并集"
        );
        assert_eq!(declared["reasoning"]["defaultEffort"], json!("low"));
    }

    /// 并集不含 none（全是有 variant 但没有 off 档的推理模型）时，`onlyReasoning` 必须与
    /// 逐模型条目同式（= !canDisableThinking）：同一能力集在 models.json 里不得有两种说法。
    #[test]
    fn auto_reasoning_without_none_in_the_union_declares_only_reasoning() {
        let pool = vec![reasoning_entry("opencode/a", &["low", "high"])];
        let declared = auto_reasoning(&pool);
        assert_eq!(declared["onlyReasoning"], json!(true), "并集不含 none 即「只推理」，与逐模型同式");
        assert_eq!(declared["reasoning"]["canDisableThinking"], json!(false));
        // 逐模型条目对同一能力集给出的是同一个值（同源语义，评审钉住的一致性）。
        let per_model = crate::reasoning::work_buddy_reasoning(&pool[0]);
        assert_eq!(declared["onlyReasoning"], per_model["onlyReasoning"]);
        // 对照：并集含 none 时两者同为 false（unions 测试钉过完整块）。
        let with_none = vec![reasoning_entry("opencode/b", &["none", "low"])];
        assert_eq!(
            auto_reasoning(&with_none)["onlyReasoning"],
            crate::reasoning::work_buddy_reasoning(&with_none[0])["onlyReasoning"]
        );
    }

    /// 豁免只给没有任何 variants 键的固定推理模型：variants 非空但全部被禁用 / 缺合法
    /// reasoningEffort 时 `prepare` 判 400，路由不得把带档位的请求改写到那种模型上。
    #[test]
    fn disabled_variants_deny_the_default_efforts_exemption() {
        let broken = json!({
            "id": "opencode/broken",
            "reasoning": true,
            "variants": { "thinking": {} },
        });
        let disabled = json!({
            "id": "opencode/disabled",
            "reasoning": true,
            "variants": { "max": { "reasoningEffort": "max", "disabled": true } },
        });
        let compatible = reasoning_entry("opencode/high", &["high"]);
        let pool = vec![broken, disabled, compatible];
        let mut request = body(AUTO_MODEL_ID, json!("hi"), Value::Null);
        request["reasoning_effort"] = json!("high");
        let mut domain = None;
        let got = select_with(&request, &pool, |n| {
            domain = Some(n);
            Some(0)
        });
        assert_eq!(
            got.as_deref(),
            Some("opencode/high"),
            "带档位的请求只落在能接住的模型上，豁免不得把空转 variants 的模型放进候选"
        );
        assert_eq!(domain, Some(1), "候选层只剩 variants 真正覆盖 high 的那一个");
    }

    /// 免费池只有 chatOnly 条目（无 images/video 声明）时不得声明视觉能力：路由的图片档
    /// 虽放行 chatOnly，但 `prepare` 随后必判 400 `unsupported_content`——这正是
    /// any_tool_capable 文档警告的「声明了却没有落点」。声明位真实存在时（chatOnly + 声明）
    /// 仍如实点亮：prepare 的图片/视频闸门对 chatOnly 条目同样只看声明位。
    #[test]
    fn a_chat_only_pool_declares_no_vision_capabilities() {
        let chat = vec![
            json!({ "id": "vendor/a", "chatOnly": true }),
            json!({ "id": "vendor/b", "chatOnly": true, "images": false, "video": false }),
        ];
        assert!(!any_tool_capable(&chat), "工具维度本就诚实（!tools 短路恰好给出 false）");
        assert!(!any_image_capable(&chat), "chatOnly 短路不得把视觉声明成 true");
        assert!(!any_video_capable(&chat), "chatOnly 短路不得把视频声明成 true");

        // chatOnly 条目真实声明了能力位时仍可点亮（prepare 对它只看声明位）。
        let chat_vision = vec![
            json!({ "id": "vendor/a", "chatOnly": true }),
            json!({ "id": "vendor/b", "chatOnly": true, "images": true, "video": true }),
        ];
        assert!(any_image_capable(&chat_vision));
        assert!(any_video_capable(&chat_vision));
    }
}
