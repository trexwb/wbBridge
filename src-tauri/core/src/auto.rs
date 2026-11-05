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
//!    `backend.rs::free_models_in` 归一出来的 `images` / `toolcall` 布尔值同形态。
//! 3. 图片与工具混合请求单列一档：`prepare` 的两个闸门（图片 → 400 `unsupported_content`，
//!    chatOnly 带 tools → 400 `tools_not_supported`）都是**服务端本地拒绝**，选错档位会让
//!    「带截图 + 带工具定义」这种常见 agent 请求注定 400。

use crate::protocol::random_uuid;
use serde_json::{json, Value};

/// 对外暴露的唯一字面量：客户端把 `model` 配成它即可获得自动路由。
pub const AUTO_MODEL_ID: &str = "WB · auto";

/// 请求是否携带图片：任一 message 的 `content` 是数组且含 `type == "image_url"` 的 part。
/// 字符串 content 恒为 false —— 与 `prepare` 的多模态解析同形态（`protocol.rs` 的 content 分支）。
pub fn has_images(messages: &[Value]) -> bool {
    messages.iter().any(|message| {
        message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts.iter().any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("image_url")
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

/// 按请求内容判定单个候选模型是否合格。
fn fits(model: &Value, images: bool, tools: bool) -> bool {
    if flag(model, "chatOnly") {
        // chatOnly 模型没有工具能力（`prepare` 会当场 400），只能服务不含 tools 的请求。
        return !tools;
    }
    if images && tools {
        return flag(model, "images") && flag(model, "toolcall");
    }
    if images {
        return flag(model, "images");
    }
    if tools {
        return flag(model, "toolcall");
    }
    true
}

/// 池里是否存在**能接工具调用请求**的候选（即 `fits(model, false, true)` 为真）。
///
/// 唯一的消费者是插件配置里那条 `WB · auto` 的 `supportsToolCall`（`sync.rs::auto_entry`）：
/// 全池都是 chatOnly 时把该字段写成 true，插件就会按工具形态发请求，而工具档没有候选 → 按 D3
/// 退回全池 → 落到 chatOnly 上被 `prepare` 当场 400，这条别名于是恒定失败。声明口径必须与
/// 选择器的档位谓词同源，否则「插件以为能给」和「核心选得出」会分叉。
pub fn any_tool_capable(models: &[Value]) -> bool {
    models.iter().any(|model| fits(model, false, true))
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

/// 按请求内容筛出候选集（能力档位，见模块头注释第 3 条）。候选层为空时退回全池。
fn candidates<'a>(body: &Value, pool: &'a [Value]) -> Vec<&'a Value> {
    let messages: &[Value] = body
        .get("messages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let images = has_images(messages);
    let tools = has_tools(body);

    let matching: Vec<&Value> = pool
        .iter()
        .filter(|model| fits(model, images, tools))
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
}
