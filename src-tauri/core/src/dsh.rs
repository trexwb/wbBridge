//! DSH（DeepSeek Harness）写入目标：把发布集写进 DSH 的 `@deepseek-ai/dsh-llm-pi-ai`
//! 路由，让 DSH 的模型列表里直接出现本服务发布的模型。
//!
//! DSH 没有 `models.json`。它由 Cordis 按层组合 profile：`$DSH_HOME/profiles/<名称>/`
//! 下的 `package.json` 声明 bundle 顺序，每个 bundle 带一份 `cordis.patch.yml`，最后叠加
//! 用户自己的 `cordis.patch.yml`（热重载）。行按 `id` 定址、**后写覆盖先写且整份替换
//! `config`**（见 `@deepseek-ai/dsh-base` 自带 patch 的头部说明）。因此本模块只改用户
//! patch 里 `id: llm-pi-ai` 那一行的 `config.providers.<本服务>` 子映射，其余行、其余
//! provider、注释一律原样保留——整份重排会丢掉用户的其它 provider（patch 不是深合并）。
//!
//! 凭据：pi-ai 的 `apiKeyEnv` 是「按请求经凭据 seam 解析的引用」，配置文件里不存密钥；
//! 引用值由 `$DSH_HOME/.credentials.yaml`（version 1，`refs:` 段）提供。本模块把**本服务
//! 自己的 api-key**（`WB_BRIDGE_API_KEY` 引用）写进那份文件——这是密钥零泄漏红线的第三处
//! 允许位，写入面仅限该文件、值仅限本服务的 Bearer 令牌，绝不进日志 / `status.json` /
//! 响应体。
//!
//! 发现只认「profile 目录里真实存在 `cordis.patch.yml`」，同 `targets.rs` 的口径：
//! 探测不到就报未检测到，绝不猜测、绝不回退到默认位置。

use crate::auto::AUTO_MODEL_ID;
use crate::json::js_trim;
use crate::model_status::client_model_id;
use crate::platform::join_host;
use crate::sync::atomic_write;
use crate::Env;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

/// 未配置 `DSH_HOME` 时的默认目录名（与 `@deepseek-ai/dsh-home-paths` 一致）。
pub const DEFAULT_HOME_DIR: &str = ".dsh";

/// pi-ai 在 `dsh-base` bundle 里的行 id（用户 patch 按这个 id 覆盖该行）。
const PI_AI_ENTRY_ID: &str = "llm-pi-ai";
/// 该行的插件名；补建整行时必须一并写出。
const PI_AI_PLUGIN_NAME: &str = "@deepseek-ai/dsh-llm-pi-ai";
/// 本服务在 DSH 里的 provider 路由键（pi-ai 要求小写连字符段）。
pub const PROVIDER_ID: &str = "wbbridge";
/// 凭据引用名（必须是 POSIX 标识符形态）；值写入 `.credentials.yaml` 的 `refs` 段。
pub const CREDENTIAL_REF: &str = "WB_BRIDGE_API_KEY";
/// 凭据文件名（与 `dsh-credentials-local` 的 `CREDENTIALS_FILENAME` 一致）。
pub const CREDENTIALS_FILENAME: &str = ".credentials.yaml";

/// 未检测到 DSH 时的状态说明（`sync.targets.dsh.reason`）。
pub const MISSING_MESSAGE: &str =
    "未检测到 DSH（DeepSeek Harness）的 profile 配置（默认 ~/.dsh/profiles/<名称>/cordis.patch.yml；可用 DSH_HOME 指定）";

/// `$DSH_HOME`：显式值（trim 后非空）> 平台主目录下的 `.dsh`。
pub fn dsh_home(env: &Env, home: &str) -> String {
    env.get("DSH_HOME")
        .map(|value| js_trim(value).to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| join_host(&[home, DEFAULT_HOME_DIR]))
}

/// 定位所有已存在的 profile 用户 patch（`profiles/<名称>/cordis.patch.yml`）。
///
/// 目录不可读、没有 profile、profile 里没有该文件——一律视为「未检测到」，返回空表。
pub fn discover_patches(env: &Env, home: &str) -> Vec<String> {
    let profiles = Path::new(&dsh_home(env, home)).join("profiles");
    let Ok(entries) = fs::read_dir(&profiles) else {
        return Vec::new();
    };
    let mut patches: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| entry.path().join("cordis.patch.yml"))
        .filter(|patch| patch.is_file())
        .map(|patch| patch.to_string_lossy().to_string())
        .collect();
    patches.sort();
    patches
}

/// 发布一轮后写 DSH：返回 `sync.targets.dsh` 的条目（形状与 `aggregate_sync` 的逐目标一致）。
///
/// - 没有任何 profile → `missing` + 原因；
/// - 全部 profile 失败 → `error` + 原因；
/// - 其余 → `ok`，`count` = 本轮发布集大小，`profiles` = 成功写入的 profile 数，
///   部分失败时附 `error`（与 `sync.error` 的「有成功就不算顶层失败」同口径）。
pub fn sync_all(env: &Env, home: &str, base_url: &str, key: &str, models: &[Value]) -> Value {
    let patches = discover_patches(env, home);
    if patches.is_empty() {
        return json!({ "status": "missing", "reason": MISSING_MESSAGE });
    }
    let credentials = join_host(&[&dsh_home(env, home), CREDENTIALS_FILENAME]);
    let mut changed = false;
    let mut written = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for patch in &patches {
        match sync_profile(patch, &credentials, base_url, key, models) {
            Ok(profile_changed) => {
                written += 1;
                changed |= profile_changed;
            }
            Err(message) => errors.push(format!("{}: {message}", profile_label(patch))),
        }
    }
    if written == 0 {
        return json!({ "status": "error", "error": errors.join("; ") });
    }
    let mut report = serde_json::Map::new();
    report.insert("status".to_string(), json!("ok"));
    report.insert("count".to_string(), json!(models.len()));
    report.insert("changed".to_string(), json!(changed));
    report.insert("profiles".to_string(), json!(written));
    if !errors.is_empty() {
        report.insert("error".to_string(), json!(errors.join("; ")));
    }
    Value::Object(report)
}

fn profile_label(patch: &str) -> String {
    Path::new(patch)
        .parent()
        .and_then(|dir| dir.file_name())
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| patch.to_string())
}

/// 写一个 profile：先改 `cordis.patch.yml`，发布集非空时再保证凭据引用就位。
///
/// 返回值是「是否真的改动了文件」。两个文件都走 `sync::atomic_write`（同目录临时文件 +
/// 原子替换，权限 0600）；内容无变化时不动盘。
pub fn sync_profile(
    patch: &str,
    credentials: &str,
    base_url: &str,
    key: &str,
    models: &[Value],
) -> Result<bool, String> {
    let original = fs::read_to_string(patch).map_err(|error| format!("读取 {patch} 失败：{error}"))?;
    let next = merge_patch(&original, base_url, models)?;
    let mut changed = next != original;
    if changed {
        atomic_write(Path::new(patch), &next).map_err(|error| error.message)?;
    }
    if !models.is_empty() {
        let current = match fs::read_to_string(credentials) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(format!("读取 {credentials} 失败：{error}")),
        };
        let next_credentials = upsert_credential(&current, key)?;
        if next_credentials != current {
            atomic_write(Path::new(credentials), &next_credentials).map_err(|error| error.message)?;
            changed = true;
        }
    }
    Ok(changed)
}

/// `models:` 里的一行：id、显示名，以及它能在 DSH 里声明哪些推理档位。
struct RouteModel {
    id: String,
    name: String,
    levels: Vec<&'static str>,
}

/// 本轮要写进 DSH 的模型：`WB · auto` 在前，其余按发布集顺序（按 id 去重）。
///
/// `WB · auto` 只在发布集非空时加——空池时它必然 400，写进去只会给用户一个坏入口。
fn route_models(models: &[Value]) -> Vec<RouteModel> {
    let mut rows: Vec<RouteModel> = Vec::new();
    if models.is_empty() {
        return rows;
    }
    rows.push(RouteModel {
        id: AUTO_MODEL_ID.to_string(),
        name: AUTO_MODEL_ID.to_string(),
        levels: auto_levels(models),
    });
    for model in models {
        let id = client_model_id(model);
        if rows.iter().any(|row| row.id == id) {
            continue;
        }
        let name = model
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&id)
            .to_string();
        rows.push(RouteModel {
            levels: declared_levels(model),
            id,
            name,
        });
    }
    rows
}

/// pi-ai 的思考档位（`THINKING_LEVELS` 去掉 `off`）。这六档的名字与本服务的 effort 名逐字
/// 相同，因此 wire 值就是档位名本身（`high: high` 原样透传）。
const PI_EFFORT_LEVELS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// 单个模型能在 DSH 里声明的档位（按 pi-ai 的顺序）。
///
/// 只声明模型真正有 variant 的档位：DSH 把未声明的档位钉成「不支持」，而声明一个选不动的
/// 档位会让请求带着 `reasoning_effort` 撞上本服务 400 `unsupported_reasoning_effort`。
/// 不推理的模型与固定推理（没有 variant）的模型一律声明 `false`——后者本来就不受档位影响，
/// 与 WorkBuddy 侧 `work_buddy_reasoning` 不给固定推理模型任何档位同口径。
///
/// `off` 刻意不声明：pi-ai 把 off 的缺席读成「支持、发送时不带参数」，而本服务不带档位时走
/// 上游模型的默认推理档（推理模型通常默认开思考），声明 Off 会给出一个关不掉的开关。
fn declared_levels(model: &Value) -> Vec<&'static str> {
    let efforts = crate::reasoning::reasoning_efforts(model);
    PI_EFFORT_LEVELS
        .into_iter()
        .filter(|level| efforts.contains_key(*level))
        .collect()
}

/// `WB · auto` 能声明的档位：发布集里任一**免费**模型可声明的档位并集。
///
/// 只在免费池上取并集——自动路由永不选付费白名单条目（数据红线 #6），把付费模型独有的档位
/// 写进来只会得到一个「选了注定 400」的入口。自动选择会按请求档位过滤候选
/// （`auto::candidates`），因此并集里的每一档都至少有一个落点。
fn auto_levels(models: &[Value]) -> Vec<&'static str> {
    let pool = crate::auto::free_pool(models);
    PI_EFFORT_LEVELS
        .into_iter()
        .filter(|level| pool.iter().any(|model| declared_levels(model).contains(level)))
        .collect()
}

/// YAML 单引号标量：内部的 `'` 写成 `''`。模型名里有冒号、`#`、`·` 时也必须安全。
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// `reasoningEfforts` 子块；`pad` 是列表项子键的缩进（即 `name:` 那一列）。
fn render_efforts(pad: &str, levels: &[&'static str]) -> Vec<String> {
    if levels.is_empty() {
        return vec![format!("{pad}reasoningEfforts: false")];
    }
    let mut lines = vec![format!("{pad}reasoningEfforts:")];
    for level in levels {
        lines.push(format!("{pad}  {level}: {level}"));
    }
    lines
}

/// 生成 `wbbridge:` 路由块（`indent` 是该键的缩进列数；其子键 +2，`models` 条目 +4/+6）。
fn render_route(indent: usize, base_url: &str, models: &[Value]) -> Vec<String> {
    let pad = " ".repeat(indent);
    let mut lines = vec![
        format!("{pad}{PROVIDER_ID}:"),
        format!("{pad}  displayName: WB Bridge"),
        format!("{pad}  api: openai-completions"),
        format!("{pad}  baseURL: {}", quote(base_url)),
        format!("{pad}  apiKeyEnv: {CREDENTIAL_REF}"),
        // 推理方言写死：baseURL 是回环地址，pi-ai 只能按猜测定方言；声明后 DSH 会把选中的
        // 档位塞进 `reasoning_effort`（openai 方言不附带 `thinking` 对象，本服务只读前者）。
        format!("{pad}  compat:"),
        format!("{pad}    thinkingFormat: openai"),
        format!("{pad}    supportsReasoningEffort: true"),
        format!("{pad}  models:"),
    ];
    for row in route_models(models) {
        lines.push(format!("{pad}    - id: {}", quote(&row.id)));
        lines.push(format!("{pad}      name: {}", quote(&row.name)));
        lines.extend(render_efforts(&format!("{pad}      "), &row.levels));
    }
    lines
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

fn significant(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && !trimmed.starts_with('#')
}

/// 一行是否就是 `- id: llm-pi-ai`（允许引号与任意缩进）。
fn is_pi_ai_entry_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    let trimmed = trimmed.strip_prefix("- ").unwrap_or(trimmed);
    let Some(rest) = trimmed.strip_prefix("id:") else {
        return false;
    };
    let id = rest.trim().trim_matches(|c| c == '\'' || c == '"');
    id == PI_AI_ENTRY_ID
}

/// 从 `key_idx` 起找该映射/列表项的结束行（下一个缩进不大于 `key_indent` 的有效行）。
fn block_end(lines: &[String], key_idx: usize, limit: usize, key_indent: usize) -> usize {
    (key_idx + 1..limit)
        .find(|&index| significant(&lines[index]) && indent_of(&lines[index]) <= key_indent)
        .unwrap_or(limit)
}

/// `lines` 区间内是否有任何有效行。
fn has_children(lines: &[String], start: usize, end: usize) -> bool {
    lines[start..end].iter().any(|line| significant(line))
}

/// 该行是否就是本服务补建出来的「空壳行」（模型清空后应整行删除，而不是留一条空路由）。
fn is_our_shell_entry(lines: &[String], start: usize, end: usize) -> bool {
    if !is_pi_ai_entry_start(&lines[start]) {
        return false;
    }
    lines[start + 1..end].iter().filter(|line| significant(line)).all(|line| {
        let trimmed = line.trim_start();
        trimmed == "config:"
            || trimmed == "providers:"
            || trimmed == "providers: {}"
            || trimmed == format!("id: {PI_AI_ENTRY_ID}")
            || trimmed == format!("name: {PI_AI_PLUGIN_NAME}")
            || trimmed == format!("name: '{PI_AI_PLUGIN_NAME}'")
    })
}

/// 把发布集合并进用户 patch 文本；返回新文本（无变化时与原文本逐字节相同）。
///
/// 只改 `id: llm-pi-ai` 行的 `config.providers.wbbridge`：
/// - 行不存在 → 追加一整行（用户 patch 允许按 id 覆盖 bundle 行）；
/// - 行在、`providers` 在 → 替换/追加本服务的子映射，其余 provider 原样保留；
/// - 行在、只有 `config:` → 在 `config` 下补 `providers`；
/// - 行在、连 `config:` 都没有 → 在行尾补 `config.providers`；
/// - 发布集为空 → 删掉本服务的子映射（连同补建的空壳行），不留死路由。
///
/// 无法安全识别的结构（`providers` 是行内非空值等）直接报错，绝不猜测性改写。
pub fn merge_patch(text: &str, base_url: &str, models: &[Value]) -> Result<String, String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let entry = (0..lines.len()).find(|&index| is_pi_ai_entry_start(&lines[index]));

    let Some(start) = entry else {
        if models.is_empty() {
            return Ok(text.to_string());
        }
        let mut appended: Vec<String> = vec![
            format!("- id: {PI_AI_ENTRY_ID}"),
            format!("  name: '{PI_AI_PLUGIN_NAME}'"),
            "  config:".to_string(),
            "    providers:".to_string(),
        ];
        appended.extend(render_route(6, base_url, models));
        lines.extend(appended);
        return Ok(join_lines(&lines));
    };

    let entry_indent = indent_of(&lines[start]);
    let end = block_end(&lines, start, lines.len(), entry_indent);
    let providers = (start + 1..end).find(|&index| {
        indent_of(&lines[index]) > entry_indent
            && lines[index].trim_start().starts_with("providers:")
    });

    let Some(providers) = providers else {
        let config = (start + 1..end).find(|&index| {
            indent_of(&lines[index]) > entry_indent && lines[index].trim_start().starts_with("config:")
        });
        if models.is_empty() {
            return Ok(text.to_string());
        }
        let route = render_route(6, base_url, models);
        match config {
            Some(config) => {
                let config_indent = indent_of(&lines[config]);
                if !inline_value(&lines[config]).is_empty() {
                    return Err(format!(
                        "无法识别 DSH 配置：第 {} 行的 config 是行内值，请先在 DSH 的模型页面保存一次",
                        config + 1
                    ));
                }
                let mut inserted = vec![
                    format!("{}providers:", " ".repeat(config_indent + 2)),
                ];
                inserted.extend(route);
                lines.splice(config + 1..config + 1, inserted);
            }
            None => {
                let shell = [
                    format!("{}config:", " ".repeat(entry_indent + 2)),
                    format!("{}providers:", " ".repeat(entry_indent + 4)),
                ];
                let mut inserted: Vec<String> = shell.to_vec();
                inserted.extend(route);
                lines.splice(end..end, inserted);
            }
        }
        return Ok(join_lines(&lines));
    };

    let providers_indent = indent_of(&lines[providers]);
    let providers_inline = inline_value(&lines[providers]);
    if !providers_inline.is_empty() && providers_inline != "{}" {
        return Err(format!(
            "无法识别 DSH 配置：第 {} 行的 providers 是行内值，请先在 DSH 的模型页面保存一次",
            providers + 1
        ));
    }
    let child_indent = providers_indent + 2;
    let providers_end = block_end(&lines, providers, end, providers_indent);
    let route_key = (providers + 1..providers_end).find(|&index| {
        indent_of(&lines[index]) == child_indent
            && lines[index].trim_start().starts_with(&format!("{PROVIDER_ID}:"))
    });

    if models.is_empty() {
        let Some(route_key) = route_key else {
            return Ok(text.to_string());
        };
        let route_end = block_end(&lines, route_key, providers_end, child_indent);
        lines.drain(route_key..route_end);
        let providers_end = providers_end - (route_end - route_key);
        if !has_children(&lines, providers + 1, providers_end) {
            lines[providers] = format!("{}providers: {{}}", " ".repeat(providers_indent));
        }
        let end = end - (route_end - route_key);
        if is_our_shell_entry(&lines, start, end) {
            lines.drain(start..end);
        }
        return Ok(join_lines(&lines));
    }

    let route = render_route(child_indent, base_url, models);
    match route_key {
        Some(route_key) => {
            let route_end = block_end(&lines, route_key, providers_end, child_indent);
            lines.splice(route_key..route_end, route);
        }
        None => {
            let insert_at = if providers_inline == "{}" {
                lines[providers] = format!("{}providers:", " ".repeat(providers_indent));
                providers + 1
            } else {
                providers_end
            };
            lines.splice(insert_at..insert_at, route);
        }
    }
    Ok(join_lines(&lines))
}

/// `key: value` 里 `value` 的裸值（`providers:` → 空串，`providers: {}` → `{}`）。
fn inline_value(line: &str) -> &str {
    line.split_once(':')
        .map(|(_, value)| value.trim())
        .unwrap_or("")
}

fn join_lines(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// 把本服务的 api-key 写进 `$DSH_HOME/.credentials.yaml` 的 `refs.WB_BRIDGE_API_KEY`。
///
/// 空文档 → 写 `version: 1` + `refs`；已有 `refs` → 就地替换/追加该引用；只有
/// `version: 1` 而没有 `refs` → 追加一个 `refs` 段。认不出的布局（缺 `version` 的
/// 预发布平铺布局、缺 `refs` 的奇怪结构）一律报错，让用户先启动一次 DSH 让它迁移。
pub fn upsert_credential(text: &str, key: &str) -> Result<String, String> {
    if key.is_empty() || key.chars().any(|c| c.is_whitespace() || c == '\'' || c == '\n') {
        return Err("api-key 形态非法，拒绝写入 DSH 凭据".to_string());
    }
    let entry = format!("  {CREDENTIAL_REF}: {}", quote(key));
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.iter().all(|line| !significant(line)) {
        return Ok(format!("version: 1\nrefs:\n{entry}\n"));
    }

    let refs = lines
        .iter()
        .position(|line| indent_of(line) == 0 && line.trim_start().starts_with("refs:"));
    if let Some(refs) = refs {
        let refs_end = block_end(&lines, refs, lines.len(), 0);
        if let Some(existing) = (refs + 1..refs_end).find(|&index| {
            indent_of(&lines[index]) == 2
                && lines[index]
                    .trim_start()
                    .starts_with(&format!("{CREDENTIAL_REF}:"))
        }) {
            lines[existing] = entry;
        } else {
            lines.splice(refs + 1..refs + 1, [entry]);
        }
        return Ok(join_lines(&lines));
    }

    let versioned = lines
        .iter()
        .any(|line| indent_of(line) == 0 && line.trim_start().starts_with("version:"));
    if !versioned {
        return Err("无法识别 DSH 凭据文件布局（缺少 version: 1）；请先启动一次 DSH 让它完成迁移".to_string());
    }
    let trailing_blank = lines.last().map(|line| line.trim().is_empty()).unwrap_or(false);
    if trailing_blank && !lines.is_empty() {
        lines.pop();
    }
    lines.push("refs:".to_string());
    lines.push(entry);
    Ok(join_lines(&lines))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    struct Scratch {
        root: PathBuf,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "wbbridge-dsh-{}-{}-{}",
                tag,
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|value| value.as_nanos())
                    .unwrap_or_default()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("临时目录可建");
            Scratch { root }
        }

        fn write(&self, relative: &str, content: &str) -> String {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().expect("有父目录")).expect("目录可建");
            fs::write(&path, content).expect("文件可写");
            path.to_string_lossy().to_string()
        }

        fn home(&self) -> String {
            self.root.to_string_lossy().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn env_of(pairs: &[(&str, &str)]) -> Env {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect::<HashMap<_, _>>()
    }

    fn models() -> Vec<Value> {
        vec![
            json!({ "id": "opencode/free", "name": "Free Model" }),
            json!({ "id": "zhipuai/glm", "name": "GLM: \"Air\" #1", "paid": true }),
        ]
    }

    #[test]
    fn discovery_follows_dsh_home_and_requires_the_patch_file() {
        let scratch = Scratch::new("discover");
        scratch.write(".dsh/profiles/desktop/cordis.patch.yml", "[]\n");
        scratch.write(".dsh/profiles/cli/package.json", "{}\n");
        let home = scratch.home();
        let default_patch = scratch.root.join(".dsh/profiles/desktop/cordis.patch.yml");
        let default_patch = default_patch.to_string_lossy().to_string();
        assert_eq!(discover_patches(&env_of(&[]), &home), vec![default_patch.clone()]);

        // DSH_HOME 覆盖默认位置；空串视作假值。
        let override_home = scratch.write("elsewhere/profiles/dev/cordis.patch.yml", "[]\n");
        let override_root = scratch.root.join("elsewhere").to_string_lossy().to_string();
        assert_eq!(
            discover_patches(&env_of(&[("DSH_HOME", &override_root)]), &home),
            vec![override_home]
        );
        assert_eq!(discover_patches(&env_of(&[("DSH_HOME", "  ")]), &home), vec![default_patch]);
        assert_eq!(discover_patches(&env_of(&[]), &scratch.write("empty", "")), Vec::<String>::new());
    }

    #[test]
    fn appends_the_route_entry_when_the_user_patch_has_none() {
        let text = "# user patch\n[]\n";
        let merged = merge_patch(text, "http://127.0.0.1:41980/v1", &models()).expect("可合并");
        assert!(merged.contains("- id: llm-pi-ai"), "{merged}");
        assert!(merged.contains("name: '@deepseek-ai/dsh-llm-pi-ai'"), "{merged}");
        assert!(merged.contains("      wbbridge:"), "{merged}");
        assert!(merged.contains("baseURL: 'http://127.0.0.1:41980/v1'"), "{merged}");
        assert!(merged.contains("apiKeyEnv: WB_BRIDGE_API_KEY"), "{merged}");
        assert!(merged.contains("- id: 'WB · auto'"), "{merged}");
        assert!(merged.contains("- id: 'OC · Free Model'"), "{merged}");
        assert!(merged.contains("- id: 'ZP · GLM: \"Air\" #1'"), "{merged}");
        // 发布集里没有推理模型 → 每一行都显式声明 false（DSH 以此把模型从推理开关里摘掉）。
        assert!(merged.contains("reasoningEfforts: false"), "{merged}");
        // 推理方言钉住 openai：本服务的 baseURL 是回环地址，pi-ai 只能靠猜。
        assert!(
            merged.contains("        compat:\n          thinkingFormat: openai\n          supportsReasoningEffort: true\n        models:"),
            "{merged}"
        );
        // 幂等：再合并一次逐字节相同。
        assert_eq!(merge_patch(&merged, "http://127.0.0.1:41980/v1", &models()).unwrap(), merged);
    }

    #[test]
    fn reasoning_levels_are_declared_per_model_and_unioned_on_auto() {
        let pool = vec![
            json!({ "id": "opencode/plain", "name": "Plain" }),
            json!({
                "id": "opencode/think",
                "name": "Think",
                "reasoning": true,
                "variants": {
                    "off": { "reasoningEffort": "none" },
                    "low": { "reasoningEffort": "low" },
                    "high": { "reasoningEffort": "high" },
                    "broken": { "reasoningEffort": "ultra" }
                }
            }),
            json!({ "id": "opencode/fixed", "name": "Fixed", "reasoning": true, "variants": {} }),
        ];
        let merged = merge_patch("[]\n", "http://127.0.0.1:41980/v1", &pool).expect("可合并");

        // 普通模型与固定推理模型都不提供档位。
        assert!(
            merged.contains("          - id: 'OC · Plain'\n            name: 'Plain'\n            reasoningEfforts: false"),
            "{merged}"
        );
        assert!(
            merged.contains("          - id: 'OC · Fixed'\n            name: 'Fixed'\n            reasoningEfforts: false"),
            "{merged}"
        );
        // 有 variant 的模型只声明真正支持的档位（顺序按 pi-ai），off 与非法档位都不出现。
        let think = "          - id: 'OC · Think'\n            name: 'Think'\n            reasoningEfforts:\n              low: low\n              high: high";
        assert!(merged.contains(think), "{merged}");
        assert!(!merged.contains("off:"), "off 不得声明（选它不会关掉思考）：{merged}");
        assert!(!merged.contains("ultra"), "非法 effort 不得声明：{merged}");
        // WB · auto 取并集：并集里的每一档都有模型能接住。
        let auto = "          - id: 'WB · auto'\n            name: 'WB · auto'\n            reasoningEfforts:\n              low: low\n              high: high";
        assert!(merged.contains(auto), "{merged}");
        // 幂等。
        assert_eq!(merge_patch(&merged, "http://127.0.0.1:41980/v1", &pool).unwrap(), merged);

        // 付费模型独有的档位不得出现在 auto 的并集里（自动路由永不选付费条目）。
        let paid_only = vec![
            json!({ "id": "opencode/plain", "name": "Plain" }),
            json!({
                "id": "opencode/paid-think",
                "name": "Paid Think",
                "paid": true,
                "reasoning": true,
                "variants": { "max": { "reasoningEffort": "max" } }
            }),
        ];
        let merged = merge_patch("[]\n", "http://127.0.0.1:41980/v1", &paid_only).unwrap();
        let auto_off = "          - id: 'WB · auto'\n            name: 'WB · auto'\n            reasoningEfforts: false";
        assert!(merged.contains(auto_off), "{merged}");
        // 付费模型自己的条目仍按它的 variant 声明档位（用户手动选中时可用）。
        assert!(merged.contains("reasoningEfforts:\n              max: max"), "{merged}");
    }

    #[test]
    fn merges_into_an_existing_pi_ai_row_without_touching_other_providers() {
        let text = "# keep me\n- id: agent-default-model\n  name: x\n  config:\n    model: beam\n- id: llm-pi-ai\n  name: '@deepseek-ai/dsh-llm-pi-ai'\n  config:\n    providers:\n      # someone else\n      acme:\n        baseURL: https://acme.example/v1\n        models:\n          - id: a\n            name: A\n- id: ui-chat\n  config:\n    transcriptView: detailed\n";
        let merged = merge_patch(text, "http://127.0.0.1:41980/v1", &models()).expect("可合并");
        assert!(merged.contains("# keep me"), "注释必须保留");
        assert!(merged.contains("# someone else"), "他人的 provider 注释必须保留");
        assert!(merged.contains("      acme:\n        baseURL: https://acme.example/v1"), "{merged}");
        assert!(merged.contains("      wbbridge:"), "{merged}");
        assert!(merged.contains("- id: ui-chat"), "{merged}");
        // 两条路由同时存在，互不吞并。
        assert_eq!(merged.matches("wbbridge:").count(), 1);
        assert_eq!(merged.matches("acme:").count(), 1);
        // 再合并一次：仍然只有一处 wbbridge，且内容稳定。
        assert_eq!(merge_patch(&merged, "http://127.0.0.1:41980/v1", &models()).unwrap(), merged);
    }

    #[test]
    fn removes_only_our_block_and_keeps_the_entry_while_other_providers_remain() {
        let text = "- id: llm-pi-ai\n  config:\n    providers:\n      acme:\n        baseURL: https://acme.example/v1\n";
        let with_route = merge_patch(text, "http://127.0.0.1:41980/v1", &models()).unwrap();
        let emptied = merge_patch(&with_route, "http://127.0.0.1:41980/v1", &[]).unwrap();
        assert!(!emptied.contains("wbbridge:"), "{emptied}");
        assert!(emptied.contains("acme:"), "他人的 provider 不得被删：{emptied}");
        assert!(emptied.contains("- id: llm-pi-ai"), "{emptied}");
    }

    #[test]
    fn removes_the_shell_entry_when_the_pool_becomes_empty() {
        let created = merge_patch("# user patch\n[]\n", "http://127.0.0.1:41980/v1", &models()).unwrap();
        let emptied = merge_patch(&created, "http://127.0.0.1:41980/v1", &[]).unwrap();
        assert!(!emptied.contains("llm-pi-ai"), "补建的空壳行必须整行删除：{emptied}");
        assert!(emptied.contains("# user patch"), "用户注释保留");
        assert_eq!(emptied, "# user patch\n[]\n");
    }

    #[test]
    fn inline_providers_map_is_handled_and_odd_shapes_are_refused() {
        let inline = "- id: llm-pi-ai\n  config:\n    providers: {}\n";
        let merged = merge_patch(inline, "http://127.0.0.1:41980/v1", &models()).unwrap();
        assert!(merged.contains("    providers:\n      wbbridge:"), "{merged}");

        let odd = "- id: llm-pi-ai\n  config:\n    providers: !!js ctx.get('x')\n";
        assert!(merge_patch(odd, "http://127.0.0.1:41980/v1", &models()).is_err());
    }

    #[test]
    fn credentials_document_is_created_updated_and_preserved() {
        let created = upsert_credential("", "abc123").unwrap();
        assert_eq!(created, "version: 1\nrefs:\n  WB_BRIDGE_API_KEY: 'abc123'\n");
        let updated = upsert_credential(&created, "def456").unwrap();
        assert_eq!(updated, "version: 1\nrefs:\n  WB_BRIDGE_API_KEY: 'def456'\n");
        assert_eq!(upsert_credential(&updated, "def456").unwrap(), updated, "无变化不重写");

        // 其它引用与 records 必须原样保留。
        let existing = "version: 1\nrefs:\n  OTHER: 'x'\nrecords:\n  llm-pi-ai/acme:\n    type: api-key\n    key: secret\n";
        let merged = upsert_credential(existing, "abc").unwrap();
        assert!(merged.contains("  OTHER: 'x'"), "{merged}");
        assert!(merged.contains("  WB_BRIDGE_API_KEY: 'abc'"), "{merged}");
        assert!(merged.contains("records:\n  llm-pi-ai/acme:"), "{merged}");

        // 只有 version、没有 refs：追加一段。
        let versioned = upsert_credential("version: 1\n", "abc").unwrap();
        assert_eq!(versioned, "version: 1\nrefs:\n  WB_BRIDGE_API_KEY: 'abc'\n");

        // 认不出的预发布布局与非法 key 一律拒绝。
        assert!(upsert_credential("DEEPSEEK_API_KEY: x\n", "abc").is_err());
        assert!(upsert_credential("", "a b").is_err());
        assert!(upsert_credential("", "").is_err());
    }

    #[test]
    fn shutdown_clear_drops_only_our_route_and_keeps_the_credential() {
        // 关停走的是同一条发布入口（`sync_all` + 空发布集）：DSH 的 `wbbridge` 路由必须整段
        // 摘掉，他人的 provider 与 `.credentials.yaml` 一律保留（红线只增改 WB_BRIDGE_API_KEY）。
        let scratch = Scratch::new("shutdown-clear");
        let patch = scratch.write(
            ".dsh/profiles/desktop/cordis.patch.yml",
            "# mine\n- id: llm-pi-ai\n  config:\n    providers:\n      acme:\n        baseURL: https://acme.example/v1\n",
        );
        let credentials = scratch.write(
            ".dsh/.credentials.yaml",
            "version: 1\nrefs:\n  OTHER: 'x'\n",
        );
        let home = scratch.home();
        let env = env_of(&[]);

        let published = sync_all(&env, &home, "http://127.0.0.1:41980/v1", "key-1", &models());
        assert_eq!(published["status"], json!("ok"));
        assert_eq!(published["changed"], json!(true));
        assert!(fs::read_to_string(&patch).unwrap().contains("wbbridge:"));
        assert!(fs::read_to_string(&credentials).unwrap().contains("WB_BRIDGE_API_KEY: 'key-1'"));

        let cleared = sync_all(&env, &home, "http://127.0.0.1:41980/v1", "key-1", &[]);
        assert_eq!(cleared["status"], json!("ok"));
        assert_eq!(cleared["count"], json!(0));
        assert_eq!(cleared["changed"], json!(true));
        let patch_text = fs::read_to_string(&patch).unwrap();
        assert!(!patch_text.contains("wbbridge:"), "{patch_text}");
        assert!(patch_text.contains("# mine"), "用户注释保留：{patch_text}");
        assert!(patch_text.contains("acme:"), "他人 provider 保留：{patch_text}");
        let credentials_text = fs::read_to_string(&credentials).unwrap();
        assert!(credentials_text.contains("  OTHER: 'x'"), "{credentials_text}");
        assert!(
            credentials_text.contains("WB_BRIDGE_API_KEY: 'key-1'"),
            "凭据引用不因清空而删除（红线只增改）：{credentials_text}"
        );

        // 重复关停/清理是幂等的：第二次不再改动任何文件。
        let again = sync_all(&env, &home, "http://127.0.0.1:41980/v1", "key-1", &[]);
        assert_eq!(again["status"], json!("ok"));
        assert_eq!(again["changed"], json!(false));
    }

    #[test]
    fn sync_profile_writes_both_files_and_reports_idempotence() {
        let scratch = Scratch::new("sync");
        let patch = scratch.write("profiles/desktop/cordis.patch.yml", "# mine\n[]\n");
        let credentials = scratch.root.join(".credentials.yaml").to_string_lossy().to_string();

        assert!(sync_profile(&patch, &credentials, "http://127.0.0.1:41980/v1", "key-1", &models()).unwrap());
        let patch_text = fs::read_to_string(&patch).expect("可读");
        assert!(patch_text.contains("wbbridge:"), "{patch_text}");
        let credentials_text = fs::read_to_string(&credentials).expect("可读");
        assert!(credentials_text.contains("WB_BRIDGE_API_KEY: 'key-1'"), "{credentials_text}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&credentials).expect("有元数据").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "凭据文件必须是 0600");
        }

        // 同一份发布集再跑：不再改动。
        assert!(!sync_profile(&patch, &credentials, "http://127.0.0.1:41980/v1", "key-1", &models()).unwrap());

        // 端口变了 → baseURL 跟着更新；门牌变了 → 凭据引用跟着更新。
        assert!(sync_profile(&patch, &credentials, "http://127.0.0.1:42000/v1", "key-2", &models()).unwrap());
        assert!(fs::read_to_string(&patch).unwrap().contains("http://127.0.0.1:42000/v1"));
        assert!(fs::read_to_string(&credentials).unwrap().contains("'key-2'"));
    }
}
