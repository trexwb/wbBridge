## Context

`/workflow` 请求：**支持 `WB · auto` 选择推理强度，根据推理强度及文本、图片、视频选择对应的模型**。

探索核实（两个 Explore 代理 + 实读源码 + 本机缓存目录/真实 status.json 取证）的现状：
- **选择器已支持档位过滤**：`auto.rs::candidates` 已按能力档位（tools/images/chatOnly）+ 请求推理档位（`requested_effort`/`accepts_effort`）过滤，空档退全池。
- **缺口 1（推理强度）**：WorkBuddy/CodeBuddy 的 `WB · auto` 别名条目**零 reasoning 声明**（`sync.rs::auto_entry`，2026-10-10 保守裁定），客户端无从选档；DSH 侧已有免费池并集口径（`dsh.rs::auto_levels`）。
- **缺口 2（图片）**：别名 `supportsImages` 恒 `false`，无 `any_image_capable` 助手；选择器图片档已存在。
- **缺口 3（视频）**：视频链路完全不存在——`models_in` 不读视频能力位、`prepare` 只放行 `image/(png|jpeg|webp|gif)` data URL、`auto.rs` 无视频探测、全仓无 video 处理。
- OpenCode `/provider` API 返回归一化后的 `capabilities.input.image/toolcall/reasoning` 形态（backend.rs:169-178，生产 status.json 证实有真实值）；video 按同一模式读 `capabilities.input.video`。

**用户裁定**：① 视频做完整端到端；② 别名推理声明用完整块（与逐模型条目同形）；③ `supportsImages` 改按免费池如实声明（推翻 2026-10-10 保守裁定）。

## 功能规格（工作流实现者的施工清单）

### 1. `src-tauri/core/src/backend.rs` — 目录视频能力位
- `models_in`（:167 镜像 `images` 的写法）：新增 `video` 字段 = `nested(model, &["capabilities", "input", "video"]) == Some(&json!(true))`。

### 2. `src-tauri/core/src/auto.rs` — 视频档 + 声明助手
- 请求探测改为 mime 感知（视频搭 `image_url` part 的 `data:video/…` data URL 进来）：`has_videos(messages)` = 任一 `image_url` part 的 url 以 `data:video/` 开头；`has_images` 相应排除视频 data URL（缺失/非字符串 url 仍按图片计，保持旧请求行为不变）。
- `fits` 重写为**子集判定**（等价重构，旧 images/tools 组合行为不变）：`chatOnly → !tools`；请求要求 images/videos/tools 时逐一要求模型对应能力位为 true。新增 `any_image_capable` / `any_video_capable`（与 `any_tool_capable` 同构）。
- 新增 `auto_reasoning(models) -> Value`：免费池上返回与 `work_buddy_reasoning` 同形可平铺对象——池内无推理模型 → `{"supportsReasoning": false}`；否则 `{"supportsReasoning": true, "onlyReasoning": false, "reasoning": {"supportedEfforts": 并集(含 none, 按 EFFORT_LEVELS 序), "defaultEffort": medium 否则首个非 none 档, "canDisableThinking": 并集含 none}}`。
- 单测：视频档路由、视频+工具/图片组合、paid-only 池的声明域、推理并集（付费独有档不进并集——镜像 dsh.rs:603 测试）。

### 3. `src-tauri/core/src/protocol.rs` — 视频部件校验
- 新增 `parse_video_data_url`（镜像 :745-759）：白名单 `video/(mp4|webm|quicktime)` + canonical base64。
- `prepare` 消息循环（:314-345）：`image_url` part 按 data URL mime 分类——`image/*` 走既有图片路径（行为/报错文案不变）；`video/*` 走视频路径：模型 `video != true` → 400 `unsupported_content` "OpenCode does not declare video input for this model"；数据 URL 非法 → "Videos must be MP4, WebM, QuickTime base64 data URLs"；通过 → `{type:"file", mime, url, filename:"message-{i}-video-{j}.<ext>"}` 进 `PreparedRequest::videos`（新访问器）。
- `backend.rs` parts 组装（:1516-1518）：text + images + videos 顺次拼接（既有 `{type:"file"}` 通道，无需新 part 形态）。
- 单测：视频白名单/非法 base64/模型不支持视频的 400、图片路径回归不变。

### 4. `src-tauri/core/src/sync.rs` — 别名声明如实化
- `auto_entry`：`supportsImages` = `any_image_capable(&pool)`（推翻恒 false）；新增 `supportsVideo` = `any_video_capable(&pool)`；平铺 `auto_reasoning(&pool)`；`maxOutputTokens` 仍不写；`maxInputTokens` 域不变（工具候选）；doc 注释同步改写（保守档裁定被推翻的条目更新为「声明域=免费池、每档都有落点」）。
- `model_entry`：新增**条件**键 `supportsVideo`（仅 `video == true` 时写入，镜像 `paid` 的条件模式——**夹具零影响**）。
- 测试更新：`the_auto_route_entry_is_appended_last_with_a_conservative_declaration` 的键序与「无 reasoning」断言改为新声明形态断言；补别名图片/视频/推理声明域测试。

### 5. 不动的部分
- `dsh.rs`（auto_levels 已是免费池并集；DSH 无视频）；`server.rs` 选择与记账（candidates 已覆盖）；`orchestration.rs`；面板（无新 status 字段）；**夹具零改动**；别名不进 status.json。

### 6. 文档与版本
- `AGENTS.md`：请求链路 WB·auto 步骤、模型发布链路、核心特例约束别名条款（保留 `supportsToolCall` 同源规则，补「图片/视频/推理声明域=免费池并集」）、基线版本表。
- `docs/contract.md` 插件配置行、`docs/wiki/核心概念与功能.md` §3.2b/c、README 若提及别名声明处。
- 版本：1.3.17 → **1.3.18**（feat 按仓库实践落在 1.3.x patch 位；1.4.x 段位已弃用）。

## 工作流设计（CreateWorkflow）

**拓扑（3 个具名子代理 + 确定性门禁）**——Rust 改动跨 4 个强耦合文件，并行实现会互相冲突，故实现为单上下文串行，验证靠确定性门禁与新鲜评审：

| 子代理 | 上下文 | 职责 | 返回 |
|---|---|---|---|
| `核心实现` | 全程持久（跨修复轮次复用） | 按规格实现全部 Rust 改动 + 单测；期间只跑触及模块的定向 `cargo test`（快档） | `{changedFiles, summary}` |
| `独立评审员` | 全新上下文，只读 | 拿 `git.diff` 审查：红线（付费域/键序契约/夹具不动/限额不放宽）、逻辑漏洞；**不改文件** | `findings[]` |
| `复核员-N` | 每个发现一个全新只读代理 | 只凭证据复现发现，产出 verified/unconfirmed | `{holds, reason}` |
| `文档同步` | 全新上下文 | 改 AGENTS.md/contract.md/wiki §3.2 + `npm run version:set -- 1.3.18`（子代理自行加载 nvm）+ `version:check` | `{editedDocs, version}` |

**门禁（world.run，确定性）**：`cargo test --manifest-path src-tauri/core/Cargo.toml`（最强档，驱动修复循环 ≤4 轮，收尾再跑一次真跑）；`cargo clippy … --all-targets -- -D warnings`；`git diff --quiet -- src-tauri/core/tests/fixtures`（夹具红线，非零即失败）。

**阶段（phase 图）**：实现核心改动 ⇄ 门禁：测试与静态检查（循环）→ 独立评审改动 → 复核评审发现（有发现才进入）→ 修复确认的发现（≤2 轮 + 重跑门禁）→ 同步文档与版本 → 终检与交付（final cargo test/clippy + `artifact.markdown` 交付报告，primary）。

**汇报**：返回 `WorkflowReport` 形状（conclusion / findings（带 evidence 与 verified 状态）/ verified / notCovered）；`notCovered` 如实列出：OpenCode 对视频 file part 的真实接受度、真实目录中是否有 `video:true` 模型——两者需实机验证。

**执行前核查**：所有门禁命令本轮已在主会话实跑过（cargo test/clippy/git diff），无需 EvalWorkflowSnippet 预演；脚本提交后若出编译诊断，按 §13 改草稿文件以 `path` 重提。

## 验证（工作流内置 + 主会话收尾）

1. 工作流内：cargo test 全绿、clippy 0 warning（-D warnings 门）、夹具 diff 为空、评审发现清零、version:check 7 处一致（1.3.18）。
2. 工作流后（主会话）：复核 AGENTS.md/文档改动、`npm run version:check` 复跑、按 AGENTS.md 交付条款核对（八组 JS 单测因未触碰 src/** 与 scripts 免跑，如实说明）。
3. 不执行 git commit / push。

## 契约影响（交付时向用户提示）

- 插件配置里 `WB · auto` 条目新增键：`supportsVideo` / `supportsReasoning` / `onlyReasoning` / `reasoning{…}`，`supportsImages` 可能由 false 变 true；逐模型条目新增条件键 `supportsVideo`。WorkBuddy/CodeBuddy 需学会读 `reasoning` 块与 `supportsVideo` 才能在 UI 上呈现选档/视频入口（插件侧属用户自己的产品，本仓库不涉及）。
- `/v1/chat/completions` 放行 `image_url` part 内的 `data:video/(mp4|webm|quicktime)` data URL（新增能力，不放宽既有 8MB/鉴权/并发红线）；base64 视频实际受 8MB 请求体上限约束（约 ≤6MB 原始体积）。
- 未验证项：OpenCode 运行时对视频 file part 的转发、上游目录是否已标 `video:true`（档位可能暂时空转，行为安全）。