# AGENTS.md — WB Bridge 桥接服务 (wbBridge)

## ⚠ 强制规范（所有 Agent 必须遵守）

**本项目是「Tauri 桌面壳 + 同进程内嵌 Rust 核心」的桥接工具。核心（`src-tauri/core/`，crate 名 `wbbridge-core`）作为 `path` 依赖静态编进壳（`src-tauri/`）里，跑在壳专用 tokio 多线程运行时上，仍在 `127.0.0.1:41980`（端口被占则由壳另择端口）提供 OpenAI 兼容 API，把**隔离托管**的 OpenCode 免费模型发布给 WorkBuddy；控制面板 `src/`（Vue 3 SFC + Vite 构建，产物 `dist/`）由 Tauri WebView 加载，经 `invoke('core_refresh' | 'core_probe' | 'core_import' | 'core_system_proxy' | 'core_shutdown' | 'core_provider_status' | 'core_set_provider_key' | 'core_clear_provider_key' | 'restart_core' | 'core_running' | 'data_dir_path' | 'read_log')` 与 `core-status` / `core-activity` / `core-failed` 事件驱动。**

> 🔴 **已彻底移除 Node 运行环境**：不再有 Node sidecar、`@yao-pkg/pkg`、esbuild 预打包，不再有 `src-tauri/binaries/`（`externalBin` 为空数组）。Node 仅用于前端构建（Vite）与 `scripts/*.mjs`。原 JS 核心与其测试已归档到仓库**之外**：
> `/Users/wbtrex/website/localServer/node/trexwb/backup/wbBridge-node-20261001/`（详见该目录 README）。
>
> 🔴 **所有 Agent（包括 file-agent、browser-agent、computer-agent 等一切主 Agent、Sub-Agent、子代理）在本项目中执行任何任务时，必须无条件遵守本 `AGENTS.md` 定义的全部规则，不得以任何理由违反。**
>
> 🔴 **文件操作根目录**：本项目所有文件操作默认以 `/Users/wbtrex/website/localServer/node/trexwb/git/wbBridge` 为根目录，**不得偏离**。
>
> 🔴 **根 `package.json` 存在且是版本单一来源**：`npm run test`（转发 cargo）、`npm run test:prefs`（面板偏好单测）、`npm run test:manifest`（更新清单单测）、`npm run test:updater-key`（签名注入包装器单测）、`npm run lint`、`npm run vite:build`、`npm run build`（= `vite:build && tauri:build && make:dmg`；`tauri:build` 是 `node scripts/with-updater-key.mjs tauri build` 的签名注入包装器）、`npm run make:dmg`（hdiutil 生成 macOS 的 `.dmg`，非 mac 平台自动跳过）、`npm run tauri:dev|build`、`npm run version:set|check`、`npm run gen:latest`（更新清单）。Rust 侧命令一律 `--manifest-path src-tauri/core/Cargo.toml` 或进 `src-tauri/`；全部 Rust 代码在 `src-tauri/` 下（壳 = `src-tauri/src/`，核心 = `src-tauri/core/`）。**不存在** Node 版的 `src/core/` 与根级 `core/`（那是已归档的 JS 核心），严禁臆造不存在的脚本、命令与路径。
>
> 🔴 **实读优先、禁止猜测**：本文件所有结论均以仓库真实代码为准。修改任何模块前必须先 `read` 读取原文，禁止凭记忆推测函数名、常量名、路由、错误码与配置字段。
>
> 🔴 **最小改动优先**：只做针对性修复，禁止重构、禁止大范围重写、禁止"顺手优化"。
>
> 🔴 **密钥零泄漏**：不得读取、打印、回显、提交或推断 `api-key`、`.env.local`、`providers.json`（用户自持的平台 Key）、用户 `~/.workbuddy/models.json` 中的凭据字段。**允许位**：核心可以把**用户自己提交进来**的平台 Key 写到数据目录的 `providers.json`（`0600`，唯一入口是 `POST /admin/set-provider-key` 的请求体，唯一读者是 `providers.rs`）；除这一处以外，任何凭据都不得落盘、出日志、进 `status.json`、进面板偏好、进子进程环境变量，或出现在任何响应体里（`provider-status` 只回「是否已配置」）。
>
> 🔴 **提交由用户决定**：Agent 完成改动后不得自动执行 `git commit` / `git push`，除非用户在本轮明确要求。


## 项目概述与定位

WB Bridge 是一个**跨平台托盘工具**，通过**隔离的 OpenCode 技术**为 WorkBuddy 提供免费模型服务。它做三件事：

1. **托管一个隔离的 OpenCode 运行时**：优先复用本机已有的 `opencode` 可执行文件，否则从 npm registry（官方源 → 国内镜像）下载官方 tarball，校验 `sha512-` 完整性后解包到数据目录，用**隔离环境变量 + 独立随机端口**以 `serve --pure` 方式启动，绝不污染用户 OpenCode 的配置、数据、缓存与登录态。
2. **对外暴露 OpenAI 兼容 API**：本地 `127.0.0.1:<port>` 提供 `/v1/models` 与 `/v1/chat/completions`（含 SSE 流式），强制请求携带鉴权头（Bearer 方案，值为数据目录 `api-key` 文件的内容）鉴权，拒绝一切浏览器 Origin，最多 8 个并发请求。
3. **把可用模型同步给 WorkBuddy**：自动发现免费模型 → 逐模型探测 → 只把探测通过的模型发布给 WorkBuddy（原子写 + 增量合并其 `models.json`，只增删自己名下的条目）。

三层结构：

| 层 | 位置 | 技术 | 现状 |
|---|---|---|---|
| 核心 | `src-tauri/core/src/`（crate `wbbridge-core`，lib `wbbridge_core`） | Rust + tokio + axum + reqwest + serde_json（`preserve_order`） | 已入库；既可独立成进程，也嵌入壳 |
| 控制面板 | `src/`（Vue 3 SFC）+ `vite.config.js` → 产物 `dist/` | Vue 3 + Vite，无 CDN、无外部请求 | 已入库 |
| 桌面壳 | `src-tauri/`（crate `wbbridge`，lib `wbbridge_lib`） | Tauri 2（Rust），托盘 + IPC + 状态轮询 | 已入库，直接 `cargo build` 出成品（无 sidecar） |

## 常用命令（安装 / 开发 / 构建 / 测试 / 签名发布）

### 环境前置

- **Rust** stable（`src-tauri/core/Cargo.toml` 与 `src-tauri/Cargo.toml` 均声明 `rust-version = "1.90"`，取锁定依赖图的实际下限）。
- **Node.js >= 24**（根 `package.json` 的 `engines`）——只为 Vite 前端构建与 `scripts/*.mjs` 服务，核心运行不依赖它。本机由 nvm 管理，**非登录 shell 的 PATH 中可能没有 `node`**，执行 npm 命令前先载入：
  ```bash
  export NVM_DIR="$HOME/.nvm"; . "$NVM_DIR/nvm.sh"
  ```
- Linux 构建需系统包：`libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libssl-dev`。

### 命令清单

| 目的 | 命令 | 执行目录 | 说明 |
|---|---|---|---|
| 安装前端依赖 | `npm install` | 仓库根 | 根 `package.json`（devDeps：`@tauri-apps/cli`、`vite`、`@vitejs/plugin-vue`、`eslint`、`@eslint/js`；deps：`vue`、`@tauri-apps/api`、`@tauri-apps/plugin-dialog`、`@tauri-apps/plugin-updater`、`@tauri-apps/plugin-process`——`src/core/bridge.js` 按需 import 这些 JS 绑定，缺任何一个 `node --test` 与 `vite build` 都会以 `ERR_MODULE_NOT_FOUND` 直接失败） |
| 运行全部核心测试 | `cargo test`（或 `npm run test`） | `src-tauri/core/` | lib 单测 + `js_parity` 对拍 + `red_lines` 红线三层；壳侧另有 `cargo test --lib`（`src-tauri/`，含 `shell_action_routes_match_the_core_contract` 逐项对齐 8 条动作） |
| 运行面板偏好测试 | `npm run test:prefs` | 仓库根 | = `node --test src/core/prefs.test.js`（白名单/凭据字段投影/视图清洗/坏数据回落/存储不可用降级/超大值拒写/静默检查节流）；只测 `src/core/prefs.js`，不联网、不编译 Rust |
| 运行操作守卫测试 | `npm run test:ops` | 仓库根 | = `node --test src/core/ops.test.js`（成功反馈文案、在飞互斥、完成冷却、拒绝节流、时钟回拨）；只测 `src/core/ops.js` 纯函数，不联网、不编译 Rust |
| 运行 AI 助手引擎测试 | `npm run test:ai` | 仓库根 | = `node --test src/core/ai.test.js`（Markdown 渲染的转义面、`maskKey`、`validateToolCall` 的未知工具/缺参/非法平台与模型 id/Key 含换行、**18 条工具表与 `TOOL_META` 自洽 + 写入/高危名单逐字钉住**、`PROVIDER_PAGES` 的主机名逐条对账壳侧 `EXTERNAL_HOSTS`、快照（含能力档位行）与 system prompt 如实转述不编数字、`validateConfig`/`validateSelfConfig` 的分工（含 `WB · auto` 免校验）、`trimRequest` 按整块裁剪不留孤儿 tool 消息、只读 `runTool` 不发请求）；只测 `src/core/ai.js` 纯逻辑，不联网、不起 Tauri |
| 运行提示词优化测试 | `npm run test:prompt` | 仓库根 | = `node --test src/core/prompt.test.js`（模板与输入上限、优化 / 迭代 / 对比请求的消息组装与数据边界、围栏与首行标签清洗、占位符缺失 / 新增比对、版本链编号、陈旧判定、提交拒绝文案与两档节流）；只测 `src/core/prompt.js` 纯函数，不联网、不起 Tauri |
| 运行对话记录测试 | `npm run test:chat` | 仓库根 | = `node --test src/core/chatSession.test.js`（IndexedDB 会话存储：字段递归裁剪、落盘前丢弃工具入参、写盘失败静默降级）；只测 `src/core/chatSession.js` 纯逻辑，不联网、不起 Tauri |
| 运行面板桥接测试 | `npm run test:bridge` | 仓库根 | = `node --test src/core/bridge.test.js`（桩 Tauri IPC：v2 监听回调收到的是事件对象，载荷在 `event.payload` 上——拿错一层 ai-* 四类事件全被滤掉、助手永远转圈）；不联网、不编译 Rust |
| 运行更新清单生成测试 | `npm run test:manifest` | 仓库根 | = `node --test scripts/gen-latest-json.test.mjs`；用临时产物目录跑真脚本，钉住六平台成功路径与缺平台/缺签名/漏架构后缀/歧义产物等失败路径。**严禁**改成真实联网或读仓库 `src-tauri/target/` 下的产物 |
| 运行 updater 签名注入测试 | `npm run test:updater-key` | 仓库根 | = `node --test scripts/with-updater-key.test.mjs`；钉住 env 文件取值优先级（进程环境 > `.env.local` > `.env` > `~/.tauri/wbBridge{,-updater}.env`，**显式空口令压过文件里的口令**、`~/…` 先展开）、内联位的两种形态（单行 base64 全文 vs `.key` 文件路径 → 读成全文并 **trim**，尾部换行会让 tauri 报 `Invalid symbol 10`）、路径注入后删除与之互斥的 `_PATH`、明文钥显式置空口令、加密钥缺口令**只告警不阻断**、公钥配对**只认配置里第一条且只告警**、`buildCommand` 的三种入参形态、CLI 在**相对路径**调用下确实执行且**任何输出都不回显密钥**。接线断言：`tauri:build` 确实经本包装器、`build` 链 `make:dmg`、`release.yml` 把私钥读成 `vars.` 且调用 `npm run tauri:build`（不再有 `tauri-action`、不再有私钥前置步骤）、`plugins.updater.pubkey` **恰好一条公钥**、`bundle.targets` **不含 dmg**。**不联网、不调 tauri CLI、不读 `~/.tauri`、不碰真实私钥**（用合成的假 base64 串与临时目录） |
| 静态检查 | `cargo clippy --all-targets` | `src-tauri/core/` | 必须保持 0 warning |
| 启动核心（独立进程，调试用） | `cargo run --manifest-path src-tauri/core/Cargo.toml --bin wbbridge-core` | 仓库根 | 监听 `127.0.0.1:41980`（`BUDDY_PORT` 覆盖），数据目录走平台默认值 |
| 开发桌面应用 | `npm run tauri:dev` | 仓库根 | `beforeDevCommand = npm run vite:dev`（`http://localhost:41990`），壳内嵌启动核心 |
| 构建前端产物 | `npm run vite:build` | 仓库根 | 输出 `dist/`（`tauri.conf.json` 的 `frontendDist` 指向 `../dist`） |
| 构建桌面应用 | `npm run build`（= `vite:build && tauri:build && make:dmg`） | 仓库根 | 产物 `src-tauri/target/*/release/bundle/`。`tauri:build` 经签名注入包装器 `scripts/with-updater-key.mjs`（把 `.env.local` / `~/.tauri` 里的私钥汇齐并归一成 `tauri build` 只认的内联 `TAURI_SIGNING_PRIVATE_KEY`）；`createUpdaterArtifacts` 为 `true`，私钥取不到时整套 Rust 编译跑完才在打包那步失败。`bundle.targets` 不含 `dmg`，macOS 的 `.dmg` 由 `make:dmg` 用 **hdiutil** 生成（非 mac 平台自动跳过） |
| 仅生成 macOS 的 .dmg | `npm run make:dmg` | 仓库根 | `scripts/make-dmg.sh`：从 `tauri.conf.json` 读 `productName`/`version`，架构取 `TARGET_TRIPLE` 的前缀（CI 交叉目标必须传，否则 `uname -m` 反映的是 runner 架构）；产物 `bundle/dmg/<productName>_<version>_<arch>.dmg` |
| 版本一致性校验 | `npm run version:check` | 仓库根 | `scripts/check-version.mjs`：7 处版本号必须一致，否则退出 1（含核心 crate） |
| 版本推进 | `npm run version:set -- <x.y.z>` | 仓库根 | `scripts/bump-version.mjs` 同步改写；执行口径见下方「版本号规则」（每次修改必须递增，豁免情形除外） |
| 健康检查 | `curl --oauth2-bearer "$(cat "<数据目录>/api-key")" http://127.0.0.1:41980/health` | 任意 | `/health` **同样需要 Bearer 鉴权**（无 key 返回 401；`--oauth2-bearer` 即发送 Bearer 头） |
| 列出已发布模型 | `curl --oauth2-bearer "<api-key 文件内容>" http://127.0.0.1:41980/v1/models` | 任意 | 返回客户端可见模型（id 形如 `<前缀> · 名称`：注册表平台 `MS`/`SF`/`TT`/`ZP`，其余 `OC`）；发布集非空时**末位**多一条合成路由 `WB · auto`（池空即不列） |

> ⚠ `<数据目录>` 取决于运行方式：**壳运行时**是 Tauri 的 `app_data_dir`（macOS `~/Library/Application Support/app.wbbridge.desktop`，identifier `app.wbbridge.desktop`）；**独立进程运行时**是平台默认目录（macOS `~/Library/Application Support/Buddy Bridge`，见下）。两套目录不要混为一谈。

### 签名与发布

- **自动更新已接线**：`src-tauri/Cargo.toml` 依赖 `tauri-plugin-updater` + `tauri-plugin-process`，`lib.rs` 的 builder 链注册两者，`tauri.conf.json` 有 `bundle.createUpdaterArtifacts: true` 与 `plugins.updater`（`pubkey` 内联公钥串 + `endpoints` 指向 `https://github.com/trexwb/wbBridge/releases/latest/download/latest.json`）。因此**本地构建必须给出签名私钥**，否则打包在生成 updater 产物那一步失败并报「A public key has been found, but no private key」。
- 构建入口 `npm run tauri:build` = `node scripts/with-updater-key.mjs tauri build`：包装器**只做注入**——按「进程环境 → 仓库 `.env.local` / `.env` → `~/.tauri/wbBridge.env`、`~/.tauri/wbBridge-updater.env` → 兜底 `~/.tauri/wbBridge-updater.key`」汇齐私钥与口令，把 `.key` 路径（含 `~/…`，Node 与 tauri 都不展开 `~`）读成内联全文并 trim，删掉与内联值互斥的 `TAURI_SIGNING_PRIVATE_KEY_PATH`，明文钥**显式**置空口令，然后 exec 目标命令。它**不判形态、不试签、公私钥配对不符也只告警不阻断**；全程只输出来源文件名与公开的 key ID，绝不回显密钥与口令。
- 产物每平台额外得到 `<安装包>.sig`：macOS `*.app.tar.gz`、Windows `*-setup.exe`（MSI 不参与更新）、Linux `*.AppImage`（`.deb` 也带 `.sig` 但不参与更新）。
- ⚠ **配置里只有一条公钥、也只有第一条生效**：`tauri-plugin-updater` 的 `verify_signature()` → `minisign-verify` 的 `PublicKey::decode()` 只读解码文本的前两行，后面的公钥被静默丢弃。**换钥必须同步换 `plugins.updater.pubkey`**，否则产物的 `.sig` 与配置公钥不配对、客户端验签必失败；老客户端只认它自己内嵌的那条，换钥那一次发布必须能让老版本先收到最后一更。当前 `plugins.updater.pubkey` 内联**恰好一条**（key ID `2B11F78BEA8A43F`），`bundle.createUpdaterArtifacts: true`。
- capabilities 声明 `updater:default` + `process:allow-restart`（**刻意不用 `process:default`**，它含 `allow-exit`，会让 WebView 绕过 `quit_app` 的优雅关停链）。
- ⚠ 变量分工：`tauri build|bundle` **只读 `TAURI_SIGNING_PRIVATE_KEY`**（值可为私钥全文或文件路径）；`TAURI_SIGNING_PRIVATE_KEY_PATH` 只对 `tauri signer sign` 生效（等价 `-f`）；`signer sign -k` 要的是**私钥字符串**，误传路径会报 `failed to decode base64 secret key: Invalid symbol 46`。
- ⚠ 空值 / 损坏 → 报错文案映射：私钥取到**空值**时报 `failed to decode secret key: incorrect updater private key password: Missing comment in secret key`（看着像口令错，其实不是）；尾部多换行 / 中间空格 → `Invalid symbol 10 / 32, offset …`；多包一层 base64 或误存公钥 → `Missing encoded key in secret key`；`.pub` 当私钥 → `failed to fill whole buffer`；加密钥 + 口令错 / 空 → `Wrong password for that key`。⚠ 「截掉尾部」与「去掉开头 4 字符」两类损坏**会被静默接受**，注入器不拦，签坏了只有客户端验签时才暴露，取值来源那几行日志要人工看一眼。
- 环境变量模板 `.env.example` → 复制为 `.env.local`（已被 `.gitignore` 忽略，**严禁提交**）。`TAURI_SIGNING_PRIVATE_KEY` 可用私钥全文或文件路径（本地推荐路径形式，密钥内容不进环境）；`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` 为私钥口令，**当前这把钥口令非空**，CI 必须配该变量，留空即 `Wrong password`；显式给空串表示这把钥确实无口令（变量只要**存在**就按显式处理，不再回落 `.env*`）。取值优先级为「进程环境 > `.env.local` > `.env` > `~/.tauri/wbBridge{,-updater}.env`」，**逐项打印来源**（混搭出问题时 tauri 只报「口令不对」，看不出哪项错）。CI 侧私钥与口令用仓库级 **Variables**（明文值，工作流绝不 echo 这两个变量；想换回 Secrets 只需把 `vars.` 改回 `secrets.`）。
- 🔴 私钥纪律：私钥绝不入库、不进日志、不进 `status.json`；公钥是公开值，内嵌配置即可。`~/.tauri` 下凭据文件的权限属用户决策项，Agent 不得自行改动。
- 密钥生成（`@tauri-apps/cli` 已在根 devDependencies，现可执行）：
  ```bash
  npm run tauri -- signer generate -p '' -w ~/.tauri/wbBridge-updater-<日期>.key
  # -k/--private-key 是「私钥字符串」，读文件要用 -f/--private-key-path；
  # 单独签文件：npm run tauri -- signer sign -f <私钥路径> -p '' <产物路径>
  ```
- `latest.json` **只有一个写者**：CI 末尾的 `update-manifest` 作业跑 `scripts/gen-latest-json.mjs`（`node scripts/gen-latest-json.mjs --dir artifacts --out latest.json`），平台键取自 artifact **目录名**里的 target triple（`macos-aarch64-…-bundles` → `darwin-aarch64`；不能靠文件名），默认要求六平台齐全、缺一即退出 1。三个 build 作业**谁都不写清单**。
  - 🔴 **url 里的空格必须写成 `.`**：GitHub 在上传时把 Release 资产名里的空格规范化成点（磁盘 `WB Bridge_1.0.2_x64-setup.exe` → API 里 `WB.Bridge_1.0.2_x64-setup.exe`），而 `/releases/download/<tag>/<空格名>`（含 `%20`）一律 404；脚本因此先 `name.replace(/ /g, '.')` 再 `encodeURIComponent`，CI 另有一步用 API 把清单 url 末段与实际资产名逐条对账，对不上即失败。
- **macOS 的 `.dmg` 不经 Tauri**：`bundle.targets` 为显式列表 `["app", "nsis", "msi", "appimage", "deb"]`（本平台不适用的目标由打包器跳过），`.dmg` 由 `scripts/make-dmg.sh`（`npm run make:dmg`，已接进 `npm run build` 链尾）用 **hdiutil** 生成 —— Tauri 自带的 create-dmg 末尾要用 AppleScript 美化窗口，无 GUI / 无 Finder 自动化授权的 runner 上会失败。脚本从 `tauri.conf.json` 读 `productName`/`version`，架构取 `TARGET_TRIPLE` 前缀（CI 交叉目标**必须传**，否则 `uname -m` 反映的是 runner 架构，会拿错 bundle 目录并把 x86_64 的包标成 aarch64），产物 `bundle/dmg/<productName>_<version>_<arch>.dmg`；非 macOS 直接跳过返回 0。
- **macOS 更新包名不含版本也不含架构**（就是 `WB Bridge.app.tar.gz`），两个 mac runner 会往同一 Release 传同名资产、后者静默覆盖前者；CI 的「给 macOS 更新包补架构后缀」步骤因此把它改名为 `WB Bridge_<arch>.app.tar.gz`（`.sig` 同步改名——minisign 签的是内容不是文件名）。
- CI：`.github/workflows/release.yml`（推 `v*` tag 或手动 dispatch）——**发布不被测试阻塞（2026-10-14 用户裁定）**：`release-gate` 作业（轻量、必须通过）跑 `check-version.mjs` + 标签↔版本闸门，三个 build 作业 `needs: release-gate`；`test` 作业照常跑 `cargo test` + 八组 `node --test`（prefs / ops / ai / prompt / chat / bridge / manifest / updater-key），但**只作告警展示、不再是发布前置**。三个平台作业跑 `npm ci` → `npm run tauri:build -- --target <triple>`（签名变量取自 `vars.TAURI_SIGNING_PRIVATE_KEY{,_PASSWORD}`，**不设私钥前置校验步骤**）；macOS 另加 `npm run make:dmg`（带 `TARGET_TRIPLE`）与「给 macOS 更新包补架构后缀」两步；`update-manifest` 作业 = 生成 latest.json → 校验清单 url 指向的资产在 Release 上真的存在 → 产物版本自检 → 上传 `latest.json`。Release 由 `softprops/action-gh-release` 创建（**不用 `tauri-apps/tauri-action`**），`tag_name` 由「读取版本号」步骤现读 `tauri.conf.json` 拼成 `v<版本>`。

### 环境变量（核心）

| 变量 | 默认值 | 作用 |
|---|---|---|
| `BUDDY_PORT` | `41980` | HTTP 端口；非 1024–65535 的整数直接失败退出（嵌入壳时由壳显式传入 `StartOptions.port`，不读环境变量） |
| `BUDDY_DATA_DIR` | 平台数据目录（见下） | 覆盖数据目录（嵌入壳时由 `StartOptions.data_dir` 注入 app_data_dir） |
| `BUDDY_MODELS_FILE` | macOS/Linux `~/.workbuddy/models.json` | 覆盖 WorkBuddy 配置文件路径（Windows 走"已保存值 → 发现"流程） |
| `BUDDY_CODEBUDDY_MODELS_FILE` | 无 | 覆盖 CodeBuddy 的 models.json 路径（优先级最高；另有对称的 `CODEBUDDY_CONFIG_DIR` / `CODEBUDDY_DATA_FOLDER_NAME`，默认目录 `~/.codebuddy`） |
| `BUDDY_NO_SYNC` | 未设置 | `=1` 时跳过启动导入与后续模型同步 |
| `BUDDY_OPENCODE_PATH` | 无 | 首选 OpenCode 可执行文件路径（优先级最高） |
| `BUDDY_PARENT_PID` | 无 | 父进程 pid；父进程消失则核心自行优雅关停 |

> 嵌入壳时**禁止**用 `std::env::set_var` 改变进程级配置（不安全、与并发冲突）；必须走 `orchestration::run(StartOptions { data_dir, port, handle_signals })`。

数据目录（`src-tauri/core/src/platform.rs::data_directory`，目录名常量 `DATA_DIR_NAME = "Buddy Bridge"`）：macOS `~/Library/Application Support/Buddy Bridge`、Windows `%APPDATA%\Buddy Bridge`、Linux `${XDG_CONFIG_HOME:-~/.config}/Buddy Bridge`。壳运行时改用 `app_data_dir`。目录内文件：

| 文件 / 目录 | 用途 |
|---|---|
| `api-key` | 32 字节随机 hex（权限 `0600`），所有 HTTP 请求的 Bearer 令牌 |
| `providers.json` | 用户自持的**平台 Key**（形态 `{ "<注册表里的 provider id>": "<Key>" }`，即请求体的 `apiKey` 落盘后的键是平台 id；权限 `0600`，经 `sync::atomic_write` 原子替换）。只由 `providers.rs` 读写；**绝不进日志 / `status.json` / 面板偏好 / 子进程环境 / 任何响应体**，`provider-status` 只回「是否已配置」 |
| `service.pid` | 单实例锁；进程仍存活即视为已在运行（独立进程按退出码 2 结束） |
| `settings.json` | 持久化 `useSystemProxy`、`workBuddyModelsFile` |
| `status.json` | 服务状态快照（UI 与外部读取的唯一状态源），串行 + 原子写 |
| `opencode.log` | OpenCode 子进程 stdout/stderr；超过 5MB 轮转为 `opencode.log.previous` |
| `runtime/<version>/opencode` | 托管下载的 OpenCode 运行时 |
| `opencode/{config,data,cache,state,project}` | 隔离的 OpenCode XDG 目录（`0700`） |

## 目录结构说明

```
wbBridge/
├── AGENTS.md                     ← 本文件（所有 Agent 的规则来源）
├── README.md                     ← 面向使用者的项目说明
├── package.json                  ← 根级便利脚本 + 版本单一来源（非 npm 工作区）
├── package-lock.json             ← 入库：CI 的 npm ci 依赖它
├── vite.config.js                ← 前端构建（root: src，outDir: ../dist，dev 端口 41990）
├── eslint.config.js              ← 扁平 ESLint（只开能真报错的规则）
├── src/                          ← 控制面板（Vue 3 + Vite；root: src）
│   ├── index.html  main.js  App.vue
│   ├── core/bridge.js            ← 与壳的唯一边界：invoke + listen → onState/onDismiss/action（另有只读 readLog/dataDir + updater/process 三调用）
│   ├── core/ops.js               ← 操作守卫唯一实现：成功反馈文案 + 在飞互斥 + 完成冷却 + 拒绝节流（纯函数，node --test 直测）
│   ├── core/ops.test.js          ← `npm run test:ops`（node --test，11 用例）
│   ├── core/update.js            ← 更新状态机（静默检查、下载进度、重启生效）；AboutView 只渲染
│   ├── core/prefs.js             ← 面板偏好的唯一读写边界（localStorage 键白名单 + 值字段投影 + 坏数据静默回落）
│   ├── core/prefs.test.js        ← `npm run test:prefs`（node --test，9 用例）
│   ├── core/ai.js                ← AI 助手引擎（纯逻辑）：工具表与 schema、校验/摘要/掩码、状态快照、system prompt、请求裁剪、受控 Markdown；网络只经 bridge.aiChat
│   ├── core/ai.test.js           ← `npm run test:ai`（node --test，18 用例）
│   ├── core/prompt.js            ← 提示词优化纯逻辑：模板、校验、优化 / 迭代 / 对比请求组装、结果清洗、占位符比对、提交守卫
│   ├── core/prompt.test.js       ← `npm run test:prompt`（node --test，22 用例）
│   ├── core/promptSession.js     ← 提示词优化会话单例：版本链、流式优化 / 对比、互斥与节流
│   ├── core/activity.js          ← 活动文案（托盘与面板共用，禁止两套文案）
│   ├── views/                    ← SideBar.vue（分组导航 + 运行设置）、ModelList.vue、ModelDetails.vue（详情右栏）、
│   │                                ServiceStatus.vue、MetricsBar.vue、FeedbackBar.vue、
│   │                                LogsView.vue、UsageView.vue、IntegrationView.vue、AboutView.vue（只读视图）、
│   │                                AssistantView.vue（AI 助手：回合循环 + 写操作逐条确认弹窗）、
│   │                                PromptView.vue（提示词优化：模板 / 版本链 / 对比测试）
│   ├── components/ModelRow.vue   styles/{variables,base}.css  public/logo.svg
├── src-tauri/                    ← 全部 Rust 代码都在这里（壳 + 核心）
│   ├── Cargo.toml                ← 壳 crate wbbridge；依赖 wbbridge-core = { path = "core" } + tokio(rt-multi-thread)
│   ├── tauri.conf.json           ← frontendDist ../dist、bundle.externalBin **空**、CSP
│   ├── capabilities/default.json ← 只声明插件侧权限（自有命令不经 capability 授权）
│   ├── build.rs
│   ├── src/{main.rs,lib.rs}      ← 托盘、IPC 命令、状态轮询、核心生命周期
│   ├── icons/                    ← 壳/托盘图标
│   └── core/                     ← Rust 核心（crate wbbridge-core，**独立 workspace**：自有 Cargo.lock 与 target）
│       ├── Cargo.toml            ← 核心 crate 版本（随产品版本同步，勿手动改；status.json 的 0.2.0 仍是历史沿革值）
│       ├── src/
│       │   ├── orchestration.rs  ← 原 main.js 的编排：启动时序、status/settings、探测调度、关停、单实例锁、StartOptions、set_exit_hook
│       │   ├── main.rs           ← 独立进程入口（--help / --version；信号自处理）
│       │   ├── lib.rs            ← 模块导出 + STAGE / PACKAGE_NAME / VERSION
│       │   ├── server.rs         ← axum 路由、Bearer 鉴权、Origin 拒绝、并发/体积上限、SSE + 心跳、/admin/*
│       │   ├── protocol.rs       ← OpenAI 兼容协议层：prepare / decode / completion / send_sse / BridgeError
│       │   ├── backend.rs        ← OpenCode HTTP 客户端：会话、事件流、原生工具审批、免费模型发现、转写钩子
│       │   ├── runtime.rs        ← 运行时发现/下载/sha512 校验/解包/隔离 env/启动/关停
│       │   ├── probe.rs          ← 模型探测（工具调用与纯文本两类；每模型各 60s 预算，重试共用）
│       │   ├── providers.rs      ← 平台注册表（四家，唯一真相）+ providers.json 凭据通道（0600、只回是否已配置）
│       │   ├── auto.rs           ← WB · auto 合成模型名的纯函数路由（按请求内容在可用池里选实际模型全限定 id；无 IO、无 JS 前身）
│       │   ├── repair.rs         ← 信封/工具格式修复与辅助模型转写（含 CLIENT_CONVENTIONS）
│       │   ├── handoff.rs        ← 原生工具 handoff：构建客户端动作、拒绝反馈、动作校验
│       │   ├── sync.rs           ← WorkBuddy models.json 原子写 + 增量合并（OWNER 标记、锁；**不写备份**，只清扫存量同族 `.bak`）
│       │   ├── workbuddy_config.rs ← models.json 定位与校验（不猜测、不创建文件）
│       │   ├── codebuddy_config.rs ← CodeBuddy models.json 定位（对照 workbuddy_config.rs）
│       │   ├── targets.rs        ← 写入目标：检测、分发、聚合
│       │   ├── system_proxy.rs   ← 系统代理解析（macOS scutil / Windows 注册表）与子进程环境
│       │   ├── reasoning.rs      ← 推理档位映射与 WorkBuddy 回退策略
│       │   ├── model_status.rs   ← 模型状态构造、请求元信息、客户端展示 ID（`<前缀> · 名称`：注册表平台 `MS`/`SF`/`TT`/`ZP`，其余 `OC`）
│       │   ├── platform.rs       ← 数据目录、路径/平台原语、运行时包名
│       │   ├── atomic.rs         ← 原子替换（重试 → 等待 → 换名，Windows 共享冲突友好）
│       │   └── json.rs           ← 容错 JSON 解析 + JS 语义原语（truthy/js_stringify/…）
│       └── tests/
│           ├── js_parity.rs      ← JS↔Rust 对拍：与 fixtures 里冻结的 expected 比较（默认无需 Node）
│           ├── red_lines.rs      ← 运行期红线守卫（鉴权/Origin/权限表/隔离配置/限额/环境白名单）
│           └── fixtures/*.json   ← 11 个模块的对拍夹具（每个带 expected 黄金快照）
├── scripts/                      ← bump-version.mjs / check-version.mjs / gen-latest-json.mjs / gen-latest-json.test.mjs / make-dmg.sh
│                                    with-updater-key.mjs（tauri:build 的签名注入包装器）/ with-updater-key.test.mjs（`npm run test:updater-key`）/ make-dmg.sh（hdiutil 出 macOS 的 .dmg）
├── docs/                         ← contract.md（壳↔核心契约）、version/（版本落点）、wiki/（功能文档）
├── .github/workflows/release.yml ← cargo 化 CI（无 sidecar 步骤）
├── .env.example / .env.local     ← 签名环境变量（.env.local 严禁提交）
└── .gitignore                    ← 忽略 node_modules、dist/、src-tauri/target|gen、.zwork/、src-tauri/binaries(防误加回)
```

> **为什么核心是 `src-tauri/core/` 而不是并进壳的单个 crate**：核心仍是独立 crate（`wbbridge-core`，
> 且是**独立 workspace**），壳通过 `path = "core"` 依赖把它静态编进同一进程。这样核心的测试
> 不必编译 tauri/webkit 依赖图（CI 的 Linux 测试任务因此无需装 libwebkit2gtk），`wbbridge-core`
> 也能单独构建出可执行文件做进程级冒烟；同时全部 Rust 代码物理位置都在 `src-tauri/` 下。
> 合并成单 crate 会把这三点全部丢掉，故不采用。

## 核心架构与数据流

### 进程模型

```
┌──────────────── Tauri 壳（单一进程）─────────────────┐
│  WebView: dist/（Vue）↔ invoke(core_* 管理命令 / restart_core / core_running / data_dir_path / read_log) / core-status │
│  托盘菜单 · 状态轮询 status.json · 退出流程              │
│  专用 tokio Runtime ──► orchestration::run(StartOptions)│
│                          │                              │
│  回环 HTTP 127.0.0.1:<port>/admin/*（壳→核心，Bearer）  │
└───────────────────────────┬─────────────────────────────┘
                            ▼
             核心编排（同进程，函数调用 + HTTP）
        ├── HTTP 服务 127.0.0.1:<port>（Bearer 鉴权、拒绝 Origin、并发 ≤8）
        ├── status.json（串行 + 原子写）→ 壳/UI 读取
        ├── WorkBuddy models.json（原子写 + 增量合并）
        └── OpenCode 子进程（隔离 env + 随机回环端口 + serve --pure）
```

- **核心不得单方面结束进程**：嵌入壳时 `set_exit_hook` 把「退出」翻译成记录 `core_code` / `core_stopped`，真正退出只由壳的**关窗 / 托盘退出 / `RunEvent::ExitRequested`** 驱动。
- **关闭窗口即退出应用（全平台一致）**：`on_window_event` 的 `CloseRequested` 在非退出态下 `prevent_close()` + `hide()`，随后在后台线程执行 `quit_app`（= `stop_core_bounded` → `cleanup_before_exit` → `exit(0)`）；**不再隐藏到托盘驻留**，用户无需二次退出。`show_main` 在 `quitting` 置位后直接返回，退出收尾期间不会被托盘左键唤回。
- **壳保留回环 HTTP 调用路径**（`admin_call` + `ACTION_ROUTES`），因此鉴权、Origin 拒绝、并发上限等红线与迁移前完全一致；`core_action` 通用代理已收口为 **8 个类型化命令**（`core_refresh`/`core_probe`/`core_import`/`core_system_proxy`/`core_shutdown`/`core_provider_status`/`core_set_provider_key`/`core_clear_provider_key`），每条只映射一个 `/admin/*` 路由、参数经 serde 类型校验，路由与载荷语义不变。
- 核心可重启：`restart_core` 关停现有实例后按新的 `StartOptions` 重新装入（`APP` 为可重置的进程级全局）。

### 启动时序（`src-tauri/core/src/orchestration.rs`）

1. 端口校验（非 1024–65535 直接失败退出，独立进程按 1）。
2. 数据目录（`StartOptions.data_dir` > `BUDDY_DATA_DIR` > 平台默认），`create_dir_all` + `0700`。
3. **单实例锁**：读 `service.pid`，若指向存活进程且不是自己 → `ALREADY_RUNNING`（独立进程退出码 2）；否则写回自身 pid。
4. 读/建 `api-key`（缺失时生成 32 字节随机 hex，`0600`）。
5. 读 `settings.json`（容错解析）、上一份 `status.json`（延续 `modelResults`）、日志 >5MB 轮转。
6. 解析 models 文件路径（`BUDDY_MODELS_FILE` → settings 已保存值 → WorkBuddy 配置目录发现；失败返回 null，**绝不静默回退**）。
7. `find_runtime` 定位或下载 OpenCode 运行时 → `start_backend` 启动子进程 → axum HTTP 服务开始监听。
8. `/agent` 校验：确认隔离配置中的 `buddy-bridge`、`buddy-chat` 两个自定义 agent 存在，否则 `phase` 不进入 `ready`。
9. 系统代理（`useSystemProxy`）：macOS `scutil --proxy`、Windows 注册表解析注入子进程 env；**解析失败回退为关闭，不阻断启动**。
10. 启动导入（`BUDDY_NO_SYNC !== "1"`）：**先清除旧 own 条目并 `drain_sync().await`，再逐模型探测（每模型各 `PROBE_TIMEOUT_MS = 60000` 预算，重试共用同一 deadline）**，仅 `ok` 的模型进入发布集。
11. 同步发布集到 WorkBuddy `models.json`，启动状态轮询/活动计时/刷新与探测后台任务；服务已在监听时启动失败会保留进程，由壳经 `/admin/shutdown` 结束这次启动。

### 请求链路（`/v1/chat/completions`）

```
WorkBuddy
  → server.rs：Bearer 鉴权（timing-safe）→ 拒绝 Origin（403）→ 限流（>8 并发 429 busy）→ 读体（>8MB 413、>20s 408）
  → WB · auto：读体之后、记账键取读之前，若 body.model == "WB · auto" 则用 auto::auto_select（池快照只取读一次）改写为实际模型全限定 id；选不出（池空）即不改写
  → protocol::prepare(body, models)：模型匹配 / messages 校验 / n=1 / tool 定义唯一性 / 图片 mime 白名单（png·jpeg·webp·gif）
  → backend::complete(...)：OpenCode 会话 + 消息 + 事件流
        ├─ 原生工具调用 → native_permissions（'*': 'ask'，question/websearch/codesearch/webfetch/task/plan_enter/plan_exit/todowrite: 'deny'）
        │    → 阻塞并交回客户端（handoff），模型不得自行执行本地动作
        └─ chatOnly 模型（buddy-chat）遇原生工具 → 502 native_tool_activity
  → protocol::decode(text, prepared)：校验信封 {content, calls}、校验工具与参数
        ├─ 一次格式修正（resend_prompt：只纠正格式）
        └─ 仍失败 → repair()：由辅助模型（translator，TRANSLATOR_ORDER 中挑，**排除刚失败的模型**）转写一次；探测路径禁止转写
  → completion() / send_sse()：非流式 JSON 或 SSE（先校验后发送，含 10s 心跳注释行）
  → 【断连兜底】backend::CompleteGuard（Drop）：放行 AbortController → 停掉每 250ms 的审批轮询 → 清 `usage_by_session`/`pending_approvals`/会话表条目 → 表空时 `stop_events()` → 派发 abort + DELETE。客户端断开时 hyper **丢弃整个 future**，写在 `.await` 之后的收尾不执行，所以清理必须挂在这上面而不是函数末尾
  → on_result：写入 status.json 计数与最近请求（客户端取消的请求绝不记为成功）
```

### 模型发布链路

```
models.json → free_models(providers)（免费判定：输入/输出/缓存全 0、输出支持文本、非 deprecated）
  → 逐模型探测（probe_model：工具调用类 + 纯文本类，失败可重试 1 次）
  → 仅 ok 的模型进入发布集（客户端 ID = `<前缀> · <name>`：注册表平台 `MS`/`SF`/`TT`/`ZP`，其余 `OC`）
  → sync_models（原子写）→ merge_models（保留非本工具条目，只清理/更新 OWNER = 'buddy-bridge-v1' 的条目）
  → 生产发布链走 merge_models_auto（`SyncOptions.auto_route = true`）：发布集非空时在末位追加一条 `WB · auto` 合成路由条目，
    使 WorkBuddy/CodeBuddy 里看得见这个入口；该条目不计入 `sync.count`，也不进 `status.json` 的模型列表
```

### 运行时隔离链路

```
find_runtime
  ├─ 优先级：BUDDY_OPENCODE_PATH > 托管目录 runtime/<version>/ > ~/.opencode/bin/（含 Homebrew 等）> Windows npm 全局 shim
  │   （拒绝 .cmd/.bat/.ps1 启动器脚本；版本必须形如 x.y.z）
  └─ 与 registry 最新版比对：本地不落后则复用；否则下载
       registry 顺序：https://registry.npmjs.org → https://registry.npmmirror.com
       校验：metadata.name/version 合法 + dist.integrity 必须为 `sha512-` + tarball 必须来自白名单 registry
       安装：下载 → sha512 校验 → 解包只取 `package/bin/<binary>` → 原子替换 → chmod 755 → 回读版本必须一致
start_backend
  → env 白名单（ENV_ALLOW：PATH/HOME/USER/LANG/TMPDIR/SHELL/SSL_CERT_FILE/NODE_EXTRA_CA_CERTS 与 Windows 必需项）
  → XDG_* 指向数据目录 + 独立随机 OPENCODE_SERVER_PASSWORD
  → 关闭 OPENCODE_DISABLE_AUTOUPDATE / _PROJECT_CONFIG / _CLAUDE_CODE / _EXTERNAL_SKILLS
  → OPENCODE_CONFIG_CONTENT = isolated_config()（权限全 ask/deny、autoupdate:false、share:disabled、两个自定义 agent）
  → 启动 `serve --pure --hostname 127.0.0.1 --port <随机空闲端口>` → 轮询 /global/health（≤120 次 × 500ms，版本必须一致）
  → 关停：SIGTERM → 等待 ≤4s → SIGKILL
```


### 关键常量（改动前必须确认语义）

| 常量 | 值 | 位置 |
|---|---|---|
| 默认端口 | `41980`（`BUDDY_PORT` 覆盖，范围 1024–65535） | `orchestration.rs`；壳 `pick_port()` |
| 请求体上限 | `MAX_BODY_BYTES = 8MB` → 413 | `server.rs` |
| 并发上限 | `MAX_CONCURRENT_REQUESTS = 8`（超出 429 busy，不得放宽） | `server.rs`；`red_lines.rs` 固定断言 |
| 请求体读取超时 | `REQUEST_BODY_TIMEOUT = 20s` | `server.rs` |
| SSE 心跳 | `DEFAULT_HEARTBEAT = 10s` | `server.rs` |
| 探测超时 | `PROBE_TIMEOUT_MS = 60_000`（每个模型一份，该模型的重试共用同一 deadline） | `probe.rs`；`orchestration.rs::start_probes` |
| 日志轮转阈值 | `5MB` | `orchestration.rs` |
| 活动去抖 | `1000ms`（紧急文案除外） | `orchestration.rs::note_activity` |
| 同步锁过期 | `LOCK_STALE_MS = 5min` | `sync.rs` |
| 健康轮询 | `120` 次 × `500ms` | `runtime.rs` |
| 关停窗口 | SIGTERM → `4s` → SIGKILL | `runtime.rs::stop_backend` |
| 权限文本截断 | `shrink_permission(value, limit)`，调用侧固定传 `400` | `backend.rs` |
| 同步归属标记 | `OWNER = "buddy-bridge-v1"` | `sync.rs` |
| status schema | `STATUS_SCHEMA_VERSION = 1`（口径：向后兼容的加法式顶层字段**不递增**，如新增的 `usage`；删除 / 改名或改变既有字段语义才同步 +1） | `orchestration.rs` |
| `status.json` 顶层 `usage` | `{since, total:{requests,ok,failed}, models:{<clientModelId>:{requests,ok,failed,lastMs,avgMs}}}`；**只累计真实请求**（`source == "request"`，客户端取消的不计为成功），跨重启延续 | `orchestration.rs` |
| 状态内置版本 | `"0.2.0"`（写入 `status.json`，历史沿革值） | `orchestration.rs` 状态初值 |
| 格式类失败集合 | `REQUEST_SHAPED_FAILURES`（4 项） | `orchestration.rs` |
| 转写首选名单 | `TRANSLATOR_ORDER`（4 项，须排除刚失败模型） | `orchestration.rs` |

## 编码规范

> 以下规范适用于本项目所有代码改动，所有 Agent 新增或修改代码时必须遵守。

### 语言与依赖（Rust 核心）

- `src-tauri/core/` 是**纯 Rust、无 Node 依赖**；edition 2021，`rust-version = "1.90"`（与壳一致；下限由锁定依赖图决定——`icu_*` ← `idna` ← `url` ← `reqwest` 需要 1.88，壳侧 tauri 2.12 一线需要 1.90）。
- 跨语言语义对齐靠 `json.rs` 的 JS 等价原语（`truthy`、`strict_eq`、`js_stringify`、`Env`）与 `serde_json` 的 `preserve_order`；**不得改用无序 map**，键序是 `models.json` 合并语义的一部分。
- 新增运行期依赖必须说明理由（体积 + 供应链可信度双重成本）；能用标准库或既有依赖完成的绝不引包。禁止把任何 Node 运行时、pkg、esbuild 重新引入核心链路。
- 异步一律 `async/await` + tokio；共享可变状态用 `Arc<Mutex<..>>` / 原子量，串行写链（`Slot` + `CHAIN_SEQ`）不得改成"看起来更快"的并发写。
- 禁止 `std::env::set_var` 配置核心实例；一切走 `StartOptions`。
- 禁止在未确认生命周期安全的前提下使用 `unwrap()`/`expect()` 于错误路径之外的 panic 风险点；`Mutex` 取值统一 `.lock().unwrap()` 仅用于「中毒即不可恢复」的既有约定写法。

### 错误与注释

- 业务错误统一 `BridgeError { message, status, code }`（`protocol.rs`），HTTP 层据此映射状态码与 `error.type`；不得吞掉错误或返回无码错误。
- 注释写**为什么**（约束、坑、平台差异、红线由来），不写"是什么"；现有源码已建立该风格。
- 平台差异集中在 `platform.rs` / `system_proxy.rs` / `runtime.rs`（`#[cfg(unix)]` / `#[cfg(windows)]`），业务代码不得散落平台判断。
- 日志只写非敏感信息；**严禁把 `api-key`、`OPENCODE_SERVER_PASSWORD`、用户凭据写入日志或 `status.json`**。

### 文件与平台

- 文件写入优先走 `atomic.rs::replace_with_retry` 或 `sync.rs::atomic_write`，禁止裸 `rename` 覆盖；Windows 需考虑 `.exe` 与不能替换运行中镜像。
- 新建目录/敏感文件显式权限（目录 `0700`、密钥文件 `0600`）。
- 路径拼接使用 `platform.rs` 提供的原语，禁止硬编码分隔符。

### 核心特例约束（发布链别名与备份清扫）

- **`WB · auto` 合成路由条目只在生产发布链写入插件配置**：`sync.rs::merge_models_auto` 走 `SyncOptions.auto_route`，该字段**默认 `false`**——JS 侧从来没有这一条，`tests/js_parity.rs::sync_options` 显式钉 `false`，因此对拍路径逐字节不变（把默认值翻成 `true`，`tests/fixtures/sync.json` 会立刻失败，那是**预期护栏生效**，不是要修的测试）。条目 `id` 刻意**绕过** `client_model_id`（否则变成 `OC · WB · auto`，与 `server.rs::chat()` 的字面比较永不匹配 ⇒ 改写不生效）。`sync.rs::owned_count` **必须排除别名**，否则 `sync.count` / `status.json` 的 `sync.targets.<目标>.count` 不再是「发布的模型数」。别名只进插件文档的 `availableModels`，**不进** `status.json` 的模型列表，面板因此没有也别新增这一行。`supportsToolCall` 必须取自 `auto::any_tool_capable(models)`，不得写成固定 `true`。
- **插件配置写入不再产生任何备份**：`sync.rs` 写路径现存的是 `sweep_old_backups(file)`——在**两条出口**都调用（`atomic_write_with` 成功之后，以及内容无变化的早退之前），按**逐段白名单**（文件名以 `<配置文件名>.buddy-bridge-` 开头、以 `.bak` 结尾、中段全是 ASCII 数字、必须是普通文件；跳过目录、`read_dir` 失败静默）只清理本工具**旧版**攒下的存量备份。不得新增备份逻辑，也不得把判定放宽成通配删除（用户自己命名的 `*.bak`、`*.tmp` 一律不碰）。写盘保护只剩三道：文件锁 + 写前二次读取 + 原子替换（同目录临时文件再 `rename`）。

### 测试规范

- 核心测试全部用 Rust：`cargo test`（`src-tauri/core/`），分三层——lib 单元测试（`src/*.rs` 内 `#[cfg(test)]`）、`tests/js_parity.rs`（JS↔Rust 对拍，比较 `tests/fixtures/*.json` 里冻结的 `expected`）、`tests/red_lines.rs`（运行期红线守卫：鉴权 / Origin / 权限表 / 隔离配置 / 限额 / 环境白名单）。**等价重构的验证方式就是对拍全绿 + `git diff --stat src-tauri/core/tests/fixtures` 为空，不得改夹具。**
- 壳（`src-tauri/`）另有 `cargo test --lib`：日志尾部读取、`shell_action_routes_match_the_core_contract`（断言壳 `ADMIN_ROUTES` 与核心 `ACTION_ROUTES` 逐项一致）、退出链路 3 项、`status_read_needed_only_skips_when_mtime_is_known`（`status.json` 轮询快路径：只在拿得到 mtime 且 `(mtime, 长度)` 未变时才允许跳过读取）、**AI 通道**（`ai_chat` 的请求形状与上限守卫、SSE 增量聚合、错误片段收口、`build_ai_body` 只在 tools 非空才声明、自托管 base URL 只回环）。
- **JS 侧八个套件都用 `node --test`（不引测试框架、不依赖 DOM）**：
  - `npm run test:prefs` = `node --test src/core/prefs.test.js`：`prefs.js` 的键白名单、值字段投影（凭据类字段绝不落 `localStorage`）、坏数据回落默认值、存储不可用/抛错时静默降级、超大值拒写、静默检查 12h 节流。
  - `npm run test:ops` = `node --test src/core/ops.test.js`：`src/core/ops.js` 的常量合法性、终态动作集合与文案表、在飞互斥文案稳定性、完成冷却窗口与时钟回拨、成功反馈文案边界。
  - `npm run test:ai` = `node --test src/core/ai.test.js`：`ai.js` 的受控 Markdown 渲染（原始 HTML 一律转义、未闭合围栏当代码块）、`maskKey`、`validateToolCall`（未知工具/缺参/非法平台与模型 id/Key 含换行）、**工具表自洽（18 条、schema 名唯一、写工具与高危工具名单逐字钉住、`TOOL_META` 全覆盖）**、`summarizeToolCall`/`toolCallPreview` 的 Key 只以掩码出现、`PROVIDER_PAGES` 的主机名必须逐条落在壳侧 `EXTERNAL_HOSTS`（跨语言对账，防「工具成功但壳拒绝」）、快照（含**能力档位**：支持工具调用 / 支持图片 / 仅对话各几个，以及 `WB · auto` 说明行）与 system prompt（含**更新固定顺序**与「做不到的事」清单）如实转述不编数字、`phaseLabel` 未知阶段原样回显、`validateConfig`（外部端点）与 `validateSelfConfig`（本机大脑：`WB · auto` 免校验、指定 id 须在名单内）的分工、`currentConfig` 默认回落（`selfModel` 默认 `WB · auto`）、`trimRequest` 按整块裁剪不留孤儿 `tool` 消息、只读 `runTool` 不发网络请求。不联网、不起 Tauri。
  - `npm run test:prompt` = `node --test src/core/prompt.test.js`：`prompt.js` 的模板与输入上限、优化 / 迭代 / 对比请求的消息组装与**数据边界**（待处理文本必须标为数据、不得变成指令）、围栏与首行套话清洗、`{{变量}}` 缺失 / 新增比对、版本链编号、源文与对比结果的陈旧判定、提交拒绝文案、500ms 前沿节流与 800ms 拒绝提示节流。不联网、不起 Tauri。
  - `npm run test:chat` = `node --test src/core/chatSession.test.js`：`chatSession.js` 的 IndexedDB 会话存储——字段递归裁剪、**落盘前丢弃工具入参**（`set_provider_key` 的 `args.apiKey` 是用户 Key 明文）、写盘失败（无 `indexedDB`、隐私模式、配额）静默降级为「这次不记住」。不联网、不起 Tauri。
  - `npm run test:bridge` = `node --test src/core/bridge.test.js`：`bridge.js` 的桩 IPC 单测——**Tauri v2 监听回调收到的是事件对象 `{event, id, payload}`**，载荷在 `event.payload` 上；`aiEvents` 必须解包后再按 chatId 过滤（拿错一层等于把 ai-chunk/ai-done/ai-error/ai-aborted 全部滤掉，助手每一轮永远转圈、连「停止」都失效）。不联网、不编译 Rust。
  - `npm run test:manifest` = `node --test scripts/gen-latest-json.test.mjs`：`latest.json` 六平台成功路径（平台键只由 artifact 目录名决定、url 空格编码、签名取自配对 `.sig`）与各类必须失败/告警的路径；用 `spawnSync` 跑真脚本 + 临时目录，**不联网、不碰真实产物**。
  - `npm run test:updater-key` = `node --test scripts/with-updater-key.test.mjs`：签名注入包装器的取值优先级、内联位两种形态的归一、互斥 `_PATH` 的删除、明文钥显式置空口令、公钥配对只认第一条且只告警、`buildCommand` 入参形态，以及接线断言（`tauri:build` 确实经本包装器、`build` 链 `make:dmg`、`release.yml` 用 `vars.` 且调用 `npm run tauri:build`、`plugins.updater.pubkey` 恰好一条、`bundle.targets` 不含 dmg）。**不联网、不调 tauri CLI、不读 `~/.tauri`、不碰真实私钥**。
  八组都已进 CI 的 `test` 作业（「运行面板偏好、操作守卫、AI 引擎、提示词优化、对话记录、桥接事件、更新清单与签名注入单测」一步；该作业自 2026-10-14 起只作告警展示，不再是发布前置）。改 `prefs.js` 的白名单/投影、`ai.js` 的工具表/校验/裁剪/渲染、`prompt.js` 的模板/校验/请求组装/清洗/守卫判定、`ops.js` 的守卫判定与文案、`gen-latest-json.mjs` 的平台识别与失败判定、或 `with-updater-key.mjs` 的取值/归一/口令/配对判定，**必须同步改对应测试**。
- **JS↔Rust 对拍已快照化**：`tests/fixtures/*.json` 每个用例带 `expected`（迁移前由 JS 实现录制、已抹平随机 id / `created` / `ms` / sync 文案；沙箱绝对路径在比较前还原成 `$BASE` 占位符，快照因此不绑定机器与目录布局）。默认不启动 Node。需要重新录制时，把仓库外归档 `backup/wbBridge-node-20261001/` 的 `core/` 与 `core-rs-tests-js/`（归档内的目录名，放回后即 `tests/js/`）放回原位，再 `WB_PARITY_RECORD=1 cargo test --test js_parity`；**禁止**在没有 JS 真相的情况下手工编辑 `expected` 来"让测试通过"。
- **测试严禁真实联网、真实下载**：网络与运行时行为必须通过注入点（`RuntimeOptions` 的 `FetchFn`/`LatestFn`/`ProbeFn`、`SyncIo`、`atomic::replace_with_retry_with`）替换。
- 新增/修改行为必须补测试；测试名要描述被保护的行为。触碰安全/供应链/数据红线时，优先在 `tests/red_lines.rs` 补断言而不是只写文档。
- 修改 `src-tauri/core/src/` 后必须运行 `cargo test` 并报告**实际**通过/失败数量，**不得以"应该能过"代替执行**；`cargo clippy --all-targets` 必须保持 0 warning。

### UI 规范

- 面板 = Vue 3 SFC + Vite 构建产物 `dist/`，由 Tauri WebView 加载；**不得引入 CDN 或任何外部请求**，必须满足 `tauri.conf.json` 的 CSP（`default-src 'self'`、`script-src 'self'`、`connect-src ipc: http://ipc.localhost`）。
- 与壳的通信只走既有契约：`src/core/bridge.js` 的 `action(name, value)` / `onState(cb)` / `onDismiss(cb)` / `aiChat(request, onDelta)` / `aiAbort()`，底层是 `invoke` **8 个 `core_*` 管理命令**（`core_refresh` / `core_probe` / `core_import` / `core_system_proxy` / `core_shutdown` / `core_provider_status` / `core_set_provider_key` / `core_clear_provider_key`，替代旧 `core_action` 通用代理） + `invoke('restart_core' | 'core_running' | 'data_dir_path' | 'read_log' | 'open_external' | 'ai_chat' | 'ai_abort')` + 事件 `core-status`（轻量快照，含顶层 `usage`）/ `core-activity`（`activity` + `modelResults` 明细，面板必须与最近一次轻量快照合并后再下发，否则逐模型状态永远为空）/ `core-failed` / `ai-chunk`·`ai-done`·`ai-error`·`ai-aborted`（按 `chatId` 分发，**不做帧级合并**，流式增量必须即时上屏）。`ai_chat` 的整份请求必须挂在 **`payload` 键**下（Tauri v2 按参数名绑定，平铺传会直接报「missing required key payload」）。新增只读命令时须同步 `src-tauri/src/lib.rs` 的 `generate_handler`；`capabilities/default.json` 对**应用自有命令无需改动**（自有命令不经 capability 授权），但**插件命令必须在此声明**——updater/process 已加 `updater:default` + `process:allow-restart`（刻意不用 `process:default`：它含 `allow-exit`，会让 WebView 绕过 `quit_app` 的优雅关停链硬杀进程）。**不得新增或改名 IPC 命令/事件**，除非同步更新 `src-tauri/src/lib.rs` 与 `src-tauri/capabilities/default.json`。
- 面板为只读展示 + 动作触发：不得在面板内直接读写文件、直接访问网络、直接调用 OpenCode。**第一例外是自动更新**：`checkUpdate/downloadUpdate/relaunchApp`（`src/core/bridge.js`）调的是 Tauri 官方 updater/process 插件，联网发生在 **Rust 侧**（插件命令），不经 WebView `fetch`，因此 CSP 的 `default-src 'self'` 无需放宽；更新状态机集中在 `src/core/update.js`，视图只渲染与触发。**第二例外是 AI 助手**：`aiChat/aiAbort`（`src/core/bridge.js`）走的是**壳侧**的 ureq 请求（`src-tauri/src/lib.rs::ai_chat`），联网同样发生在 Rust 侧、不经 WebView `fetch`，因此 CSP 无需放宽；协议与工具语义集中在 `src/core/ai.js`，视图只负责渲染与确认。**提示词优化复用同一条 AI 通道**（同样是 `aiChat/aiAbort`），不新增第二类网络入口；壳侧只有一个 `activeChatId`，因此它与 AI 助手必须互斥。
- 动作执行期间必须置忙（按钮禁用 + spinner + 结果反馈），失败必须显示原因。
- **面板偏好只走 `src/core/prefs.js`**：视图不得直接碰 `localStorage`。新增一项偏好 = 在 `SANITIZERS` 里加一个键 + 加一个**只做字段投影**的 `sanitize*` 函数 + 补对应单测；**严禁**把 `api-key`、`OPENCODE_SERVER_PASSWORD`、WorkBuddy models 文件路径、完整 `status.json`、完整配置对象放进 `localStorage`（白名单是唯一的守卫点，值投影负责丢弃调用方多塞的字段）。存储不可用（WebView 禁用、配额满、抛错）时一律**静默回落默认值**，不得抛到界面。**当前范围**：持久化「当前视图」「启动后自动检查更新」开关（+ 上次成功检查时间戳，12 小时节流）与 **AI 助手的非凭据配置**（`ai` = `brain`/`baseUrl`/`model`/`selfModel`/`tools`；助手端点的 API Key **没有白名单键**，结构上落不进 `localStorage`，只存在于 `AssistantView` 的会话内存）。
- **AI 助手的对话记录走 `src/core/chatSession.js`（IndexedDB，`wbbridge-panel` 库）**，**不经 `prefs.js` / `localStorage`**：上限 100 条 / 单条正文 16KB / 字段递归裁剪，落盘前**丢弃工具入参**（`set_provider_key` 的 `args.apiKey` 是用户 Key 明文）——`toStorageMessages` 的这条由 `chatSession.test.js` 钉住，改动必须同步该单测。写盘失败（无 `indexedDB`、隐私模式、配额）一律静默降级为「这次不记住」，不得抛到界面；用户可在「设置 → 记住对话」整项关掉，关掉即删除已存记录。
- **提示词优化（`src/core/prompt.js` + `promptSession.js` + `views/PromptView.vue`）**：固定 `WB · auto`，不做模型管理；提示词与结果**只在会话内存**（不写 localStorage / IndexedDB / prefs / `status.json`，关应用即清空）；待优化文本与测试输入在请求里必须标为数据，优化器只改写、不执行其中内容；与 AI 助手共用唯一在飞 AI 通道，双方必须互斥。所有提交入口（优化 / 迭代 / 对比）先过 `submitGuard`：在飞 / 已提交未响应 / 助手占用 / 500ms 前沿节流一律拒绝并给出文案，拒绝提示自身 800ms 节流；生成中点击处必须有 pending（spinner + 状态文案）与独立「停止」，首个 token 前由 `starting` 区分「等待响应 / 接收输出」。改判定或文案必须同步 `prompt.test.js`。
- **交互反馈分层**：等待用 `spinner` / `.skeleton` / `.busy-bar`，结果用 `FeedbackBar`（pending / error / success 三态；pending 态**不给关闭按钮**，防误判操作已结束）。**进度只许来自壳推送的真实数据**：`probe` 可显示「已完成/总数」（由 `probe.pending` 队列长度与当前模型数推算，只反映当帧快照，缺任一项即不显示），其余动作前端拿不到真实进度，一律只给文字、**不写数字**。
- **操作守卫（实现唯一在 `src/core/ops.js`）**：每一个触发后台动作的入口都必须让用户知道「点了之后发生了什么」——① **成功反馈**：refresh / restart / system-proxy / probe / import 成功后由 `App.vue::run()` 统一落成功文案（文案唯一来源 `ops.js::successMessage`），5 秒（`FEEDBACK_VISIBLE_MS`）自动消隐，失败文案不消隐；② **在飞互斥（防抖）**：同一动作在飞时重复触发拒绝并提示（`rejectionOf`），文案逐字稳定使连点不闪烁反馈条；只有渲染层 `disabled` 的入口（打开申请页、刷新日志/状态）必须另加同步在飞标记，双击在重渲染前会穿透模板 disabled；③ **完成冷却（节流）**：终态动作（`COOLDOWN_ACTIONS` = refresh / restart / system-proxy / import）成功后 5 秒（`ACTION_COOLDOWN_MS`）内重复触发拒绝并提示「刚完成」，失败不进冷却；④ 受控开关（侧栏系统代理）被拒绝时必须回弹 DOM 视觉状态，不得停留在假位置。改动守卫判定或文案必须同步 `ops.test.js`。
- **图标一律内联 SVG**（`stroke="currentColor"`）：禁止字符符号（▦▤◔⇄ⓘ 之类随系统字体变化、基线不齐、无法统一线宽），也禁止外部图标资源或图标字体（CSP `default-src 'self'`）；**每个 `<svg>` 都必须有显式宽高**（由宿主类的 `> svg` 规则给，或自带属性）——HTML 里不带宽高的 inline SVG 默认按 300×150 渲染，会把整行布局顶坏，而这种破损在 `vite build` 与 eslint 上都不留痕迹。
- **样式硬约束**：颜色 / 时长 / 阴影一律取 `src/styles/variables.css` 的 token（交互层含 `--dur-*`、`--ease-*`、`--focus-ring`、`--on-primary`、`--shadow-s/m`、`--nav-hover-bg` / `--nav-active-bg` / `--nav-active-ring`；语义色含 **`--danger` / `--danger-bg`——那是「高危写入」专用，与普通「写入」用的 `--orange` 必须分属两个色相，同系看着分不出等级），组件内不得写死数值；**小字号说明文字一律取 `--muted-strong`**（`--muted` 只可用于 ≥3:1 的非文本用途，实测 12–13px 正文用它只有 3.66–4.01:1，不达标）；`prefers-reduced-motion` 下位移与淡入必须取消，但 `spinner` / `.skeleton` / `.busy-bar` 的循环**必须保留**（否则「正在加载」失去唯一载体）；所有可点元素必须有 hover、`focus-visible` 焦点环与 `disabled` 态。
- **弹窗与助手的三条界面不变量**（`src/views/AssistantView.vue`，改这里不得回退）：① 设置与提案确认两个弹窗都要有 **Tab 焦点陷阱**（首尾环回，可聚焦集合用 `getClientRects().length > 0` 过滤——弹窗整块在 `position: fixed` 遮罩里，`offsetParent` 在这条链上各引擎取值不一致）+ 关闭后**焦点归还**给打开它的控件（程序化打开没有触发元素，回落输入框）；`role="dialog"` 必须挂 `aria-labelledby` 指向 `<h3>`；`Esc` 收口顺序固定为「先关提案确认（等同全部取消），再关设置」。② **失败原因不得拼进气泡正文**——正文会被 `apiHistory()` 回灌给下一轮，模型会把「⚠ 请求失败」当成自己说过的话复读或道歉；失败/停止各占独立元素与 `role="alert"` / `role="status"`。③ 粘底滚动只在用户本来贴底时跟随，**上翻时必须给出「回到最新」入口**，且该入口放在滚动容器**之外**的视图流内（纵向 flex 滚动容器里的 sticky 表现不可靠）。④ **会话状态（`messages` / `chatting` / `confirming` / `draft`）必须是 `chatSession.js` 的模块级单例**：面板用 `v-if` 切视图会卸载组件，状态放组件里会让「生成中切走」直接丢内容；`onBeforeUnmount` **不得**调 `abortChat()`（切走不等于中止，中止只由「停止」按钮决定）。工具执行读的快照取 `chatSession.coreSnapshot`（由 `App.vue` 每帧喂），不能取卸载后停更的 `props.state`。⑤ **模型就位前禁用输入**：门槛 `gate` 要求本机大脑「核心 `ready` + 至少一个探测通过的模型」（池空时 `WB · auto` 无落点）、外部端点「地址 / 模型 / Key 齐备」；禁用时必须同时给出原因与出路按钮，不得只留一个点不动的输入框。

#### 布局与窗口

- **布局**：外层 `.shell` 为左右两段——左侧栏固定宽 `var(--sidebar-w)` + 右侧主区；主区默认单列，选中模型时 `.content.is-split` 变为
  `grid-template-columns: minmax(0, 1fr) var(--details-w)`（`src/App.vue`）。
- **详情面板是右侧常驻分栏，不是浮层**：无遮罩、不 `position: fixed/absolute`、不覆盖列表；**任何窗口宽度都不降级为上下堆叠**。
  禁止新增按宽度堆叠的媒体查询或 `<900px` 降级分支——`src/` 内现存的媒体查询只有 `prefers-color-scheme`（`variables.css`）与
  `prefers-reduced-motion`（`base.css`）两处。收起详情有三条等价路径：`Esc`（`App.vue::onKeydown`）、详情头部「收起详情」按钮、
  窗口失焦（`onDismiss`）；收起即清空选中，列表占满主区宽度。
- **侧栏是分组导航**：分组为「模型 / 运行 / 集成 / 其他」，底部为「运行设置」（系统代理开关 + 版本号）。**8 个入口都是已实现的视图**（视图 ID 的唯一名单是 `prefs.js` 的 `VIEW_IDS`，视图状态由 `src/App.vue` 的 `view` 持有，侧栏按 `current` 高亮并带 `aria-current="page"`）：
  模型与服务（`.top` + `.content`，保留主从两栏）、平台（`src/views/ProvidersView.vue`，填写/清除四家平台 Key，只显示「已配置 / 未配置」）、运行日志（`src/views/LogsView.vue`，经 `read_log` 只读拉取日志尾部）、用量与额度（`src/views/UsageView.vue`，渲染 `status.json` 顶层 `usage`）、
  WorkBuddy 集成（`src/views/IntegrationView.vue`，只读展示配置定位与最近发布结果，唯一动作是复用既有 `import`）、**提示词优化**（`src/views/PromptView.vue` + `src/core/prompt.js` + `src/core/promptSession.js`：固定本机 `WB · auto`；系统 / 用户提示词、7 个内置模板、版本链与迭代、占位符缺失 / 新增提示、「同一输入分别跑源提示词 / 当前版本」的对比测试；内容只存会话内存；与 AI 助手共用壳侧 AI 通道并互斥；提交有在飞互斥、500ms 前沿节流与逐次拒绝文案）、**AI 助手**（`src/views/AssistantView.vue` + `src/core/ai.js`：自然语言入口，只读工具立即执行、写入工具一律先作提案再逐条确认；大脑默认本机 `WB · auto`，也可切外部 OpenAI 兼容端点；端点 Key **只在会话内存**（见 `prefs.js` 的 `ai` 键白名单），**对话记录默认记在本机 IndexedDB**（`src/core/chatSession.js`，可在「设置」关掉、可「清空」；工具入参不落盘）；**模型就位前输入区整块禁用**，横幅给出原因与出路）、关于与更新（`src/views/AboutView.vue`，版本与数据目录只读 + **自动更新区**：启动后静默检查、发现新版本才出现更新条、由用户点「下载并安装」，进度百分比只在上游给出 `contentLength` 时显示，装完点「重启应用」经 `process.relaunch` 走 `ExitRequested` 的有界停止）。
  **禁止**再出现「规划中」标签、禁用占位或可点击却无响应的假入口。
- **`usage` 口径（`status.json` 顶层字段）**：`{"since": ISO, "total": {requests, ok, failed}, "models": {"<clientModelId>": {requests, ok, failed, lastMs, avgMs}}}`；只累计 `source == "request"` 的真实客户端请求——探测（`probe`）不计；客户端取消（连接断开）走 `server.rs` 的两条早退路径，**整条不计入**（`requests` 也不 +1，不是记为失败）；启动时从上一份 `status.json` 读回，与 `modelResults` 同法跨重启延续。
- **`schemaVersion` 升级口径**：向后兼容的**加法式**顶层字段（旧壳忽略未知字段即可）**不**递增两侧 `STATUS_SCHEMA_VERSION`，本次保持 `1`；只有删除 / 改名顶层字段、或改变既有字段语义（非兼容变更）时，才把 `src-tauri/src/lib.rs` 与 `src-tauri/core/src/orchestration.rs` 两侧常量**同步 +1**（规则原文见两处注释）。
- **设计 token**：尺寸与颜色一律取自 `src/styles/variables.css`；小字号说明文字（副标题、页脚说明、耗时行等）用 `--muted-strong`，
  普通次要文字用 `--muted`，不得在组件里写死颜色或宽度。

| 布局常量 | 值 | 位置 |
|---|---|---|
| 默认窗口 | `1120 × 720` | `src-tauri/tauri.conf.json` → `app.windows[0]` |
| 最小窗口 | `860 × 560`（`minWidth` / `minHeight`） | 同上 |
| 侧栏宽 | `--sidebar-w: 208px` | `src/styles/variables.css` |
| 详情栏宽 | `--details-w: clamp(300px, 45%, 360px)` | 同上 |
| 宽度断点 | **无**（不按窗口宽度堆叠，见上） | `src/` 内无宽度媒体查询 |

### 命名规范

| 类别 | 规则 | 示例 |
|---|---|---|
| Rust 文件/模块 | snake_case，单词表意 | `workbuddy_config.rs`、`model_status.rs` |
| Rust 函数/变量 | snake_case，动词开头表意 | `usable_models()`、`sync_published()`、`resolve_models_file()` |
| Rust 常量 | SCREAMING_SNAKE_CASE | `PROBE_TIMEOUT_MS`、`TRANSLATOR_ORDER`、`MAX_BODY_BYTES`、`OWNER` |
| Rust 类型 | CamelCase | `BridgeError`、`PreparedRequest`、`SyncOptions` |
| 对外契约 | 保持 JS 侧原名（snake_case 化） | 路由 `/v1/chat/completions`、`/admin/system-proxy`；错误码 `invalid_model_output` |
| Vue 组件文件 | PascalCase；视图放 `src/views/`，通用组件放 `src/components/` | `views/SideBar.vue`、`views/ModelDetails.vue`、`components/ModelRow.vue` |

## Git 提交规范

提交格式：`type(scope): content`

| type | 说明 |
|---|---|
| `feat` | 新功能 |
| `fix` | 修复 |
| `docs` | 文档 |
| `style` | 格式调整（不影响逻辑） |
| `refactor` | 重构 |
| `perf` | 性能优化 |
| `test` | 测试 |
| `chore` | 构建 / 工具 / 依赖 |

`scope` 约定：`core`（`src-tauri/core/`）、`ui`（`src/` 面板与 Vite）、`tauri`（壳与 `src-tauri/icons/`）、`scripts`（版本脚本）、`docs`（文档）、`ci`（工作流）。

示例：`refactor(core): 移除 Node sidecar，核心改为 Rust 内嵌`

提交纪律：

- 提交前必须运行 `cargo test`（`src-tauri/core/`）；测试失败禁止提交。
- **标签纪律（发布必须遵守）**：① 先 `npm run version:set -- <x.y.z>` 改写 7 处落点并**提交**，再**在该提交上**打 `v<x.y.z>` 标签——标签绝不允许指向"不含本次版本推进"的提交；② CI 的 `release-gate` 作业有闸门（2026-10-14 起从 test 作业拆出，test 失败不再阻塞发布，此闸仍必须通过）：`GITHUB_REF_NAME` 去掉前导 `v` 必须等于 `tauri.conf.json` 的 `version`，不一致 exit 1（`check-version.mjs` 只看同一提交内部一致，抓不到标签指错）；③ 三个 build 作业上传前打印产物名，`update-manifest` 作业汇总六平台安装包名并把「产物版本 = tag 版本」自检结论写进 run summary，**待人工核对项不为 0 时不得点 Publish**；④ 删除/移动**已推送**的标签属共享状态变更，只能用户执行。
- 一次提交只包含一类改动，禁止把源码改动与大批二进制/图标混在一起。
- **严禁提交**：`api-key`、`.env.local`、`status.json`、`settings.json`、`node_modules/`、`dist/`、`src-tauri/target/`、`src-tauri/core/target/`、`src-tauri/gen/`、`.zwork/`、任何私钥文件、以及仓库外的 Node 归档目录。
- 仓库现状：分支 `dev`，远端 `origin = https://github.com/trexwb/wbBridge.git`（另有 `main`）。提交前务必 `git status` 复核实际纳入内容。

## 红线与限制

### 安全红线（不得移除、不得放松、不得绕过；由 `tests/red_lines.rs` 部分钉死）

1. **鉴权**：所有路由（含 `/health`）必须校验鉴权头（Bearer 方案，值 = 数据目录 `api-key` 文件内容），比较走 `server.rs::constant_time_eq`（长度不等直接 false，等长时用 `subtle::ConstantTimeEq`；**不得**换成手写异或累加，后者可能被优化成提前短路）；`api-key` 文件权限 `0600`，**缺失或内容为空白时随机生成并强制 `0600`**（沿用空白文件等于把期望头退化成空 Bearer 值）；`authorized` 对空密钥一律拒绝；裸 key / 非 Bearer 方案一律 401。
2. **拒绝浏览器来源**：请求带任何非空 `Origin` 头一律 403。WorkBuddy 只经其原生运行时访问本服务。
3. **仅监听回环地址**：`TcpListener::bind(("127.0.0.1", port))`（`orchestration.rs`），禁止改为 `0.0.0.0` 或对外开放。
4. **限流与体积**：并发 ≤8、请求体 ≤8MB，不得放宽（并发 2026-10-10 经用户裁定由 4 调至 8，其余一律维持「不得移除、不得放松、不得绕过」）。
5. **原生工具严禁开放给模型**：`native_permissions()` 必须保持 `'*': 'ask'`，且 `question`/`websearch`/`codesearch`/`webfetch`/`task`/`plan_enter`/`plan_exit`/`todowrite` 为 `deny`；`isolated_config()` 的 `autoupdate: false`、`share: "disabled"` 不得改动。**模型绝不允许直接执行本地动作**，原生动作只能以 handoff 交回客户端。
6. **chatOnly 模型不得使用工具**：纯文本 agent（`buddy-chat`）遇原生工具活动必须报 `native_tool_activity`，不得降级为静默执行。
7. **密钥零泄漏**：子进程环境只透传 `ENV_ALLOW` 白名单（其中不得出现任何凭据类变量名）；`OPENCODE_SERVER_PASSWORD` 只能用核心自己生成的值；密钥不得写入日志、`status.json` 或仓库。**唯一的允许位**：`providers.rs` 可把**用户自己提交**的平台 Key 原子写到数据目录 `providers.json`（`0600`），入口只有 `POST /admin/set-provider-key` 的请求体；该 Key 不得出日志、出 `status.json`、出任何响应体——`/admin/provider-status` 的返回形态只有 `{ providers: [{ id, label, configured }] }`，**连掩码尾字符都不返回**（由 `providers.rs::status_never_contains_key_material`、`stored_file_holds_the_key_but_the_status_shape_does_not` 与 `red_lines` 的注册表守卫三条钉死）。后续 Stage 3 注入子进程时同样只能走 `OPENCODE_CONFIG_CONTENT`，**不得**把 Key 放进 `ENV_ALLOW` 或独立环境变量。

### 供应链红线（OpenCode 运行时）

1. 只能从**白名单 registry**（`registry.npmjs.org`、`registry.npmmirror.com`）获取运行时元数据与 tarball；元数据必须满足 `name` 匹配、语义化 `version`、`dist.integrity` 以 `sha512-` 开头、tarball 来源属于白名单。
2. **下载后必须校验 sha512 完整性**，校验失败不得使用、不得"跳过校验重试"；镜像与官方字节不一致时必须报错（测试已固化该行为）。
3. 解包只提取 `package/bin/<binary>` 单个文件，禁止整包解开到运行时目录。
4. 安装后必须**回读 `--version` 与元数据版本一致**，不一致即失败；禁止把无法确认版本的候选当作运行时。
5. 拒绝 `.cmd` / `.bat` / `.ps1` 启动器脚本作为运行时二进制。
6. 不得把运行时降级为"直接调用系统 `opencode` 的共享实例"——隔离（独立 XDG 目录、独立端口、独立密码、独立配置）是本项目的核心价值。

### 数据红线

1. 写 WorkBuddy `models.json` 时：**只清理/更新 `OWNER='buddy-bridge-v1'` 名下的条目，其他条目与对象元数据必须原样保留**；手动条目 ID 冲突时不得擅自覆盖。
2. `status.json` 写入必须串行 + 原子；探测中的临时状态不得覆盖已完成状态。
3. `service.pid` 单实例锁不得删除或绕过；重复启动必须失败（独立进程 `exit(2)`）而不是抢占端口。
4. 格式类上游失败（`invalid_model_output` / `invalid_tool_call` / `native_tool_activity` / `output_truncated`）**不得撤销已发布模型**；探测路径**不得启用辅助模型转写**；转写候选**不得包含刚失败的模型本身**。
5. 客户端取消的请求不得记录为成功。

### 工程红线

1. **禁止重构**：只做针对性最小改动；不得重命名已有导出、路由、错误码、IPC 命令与事件名。
2. **禁止重新引入 Node 运行期到核心**：不得为 `src-tauri/core` 加 sidecar/pkg/esbuild 链路，也不得新建第二个核心实现。
3. **禁止在仓库内写入运行时数据**：所有运行期文件（`api-key`、`status.json`、日志、runtime、opencode 目录）只能写入平台数据目录（或壳的 `app_data_dir`）。
4. **禁止编造已验证状态**：不得把未实测的行为写成已验证；没有实机 / CI 证据支撑的说明必须标注「未验证」，未走通的链路（如自动更新的完整升级闭环）必须如实写明。
5. **禁止大规模二进制入库**：`src-tauri/binaries/` 已随 sidecar 方案废弃（目录已删除，`.gitignore` 仍保留守卫条目），不得复活该约定。
6. **版本推进必须依规执行、不得遗漏**：见下方「版本号规则」——每次完成的修改都必须按规则递增版本号；唯一豁免是修复上一轮因理解错误造成的问题（返工）。
7. **Rust 代码只能放在 `src-tauri/` 下**：壳 = `src-tauri/src/`，核心 = `src-tauri/core/`（独立 workspace、壳以 `path = "core"` 依赖）。不得在仓库根重建 `core-rs/` 之类的第三个 Rust 位置，也不得把核心并进壳的单个 crate（那会让核心测试被迫编译 tauri/webkit 依赖图、失去独立二进制）。

## 当前基准版本

| 项 | 值 | 来源 |
|---|---|---|
| 版本单一来源 | **1.2.3** | 根 `package.json` 的 `version` |
| 壳工程同步落点 | **1.2.3** | `src-tauri/tauri.conf.json` 与 `src-tauri/Cargo.toml` 的 `[package] version` |
| 核心 crate 版本 | **1.2.3** | `src-tauri/core/Cargo.toml`（`wbbridge-core --version` 输出；随产品版本同步，`version:set`/`version:check` 均覆盖） |

> `status.json` 内写入的 `version` 是历史沿革值 `0.2.0`，**不属版本落点**，不得在无用户指令的情况下静默改动（见「关键常量」）。

**版本号规则（写死，所有 Agent 必须遵守）**：

- 版本号以**根 `package.json`** 为唯一来源；`src-tauri/core/Cargo.toml` 随产品版本同步（`version:set`/`version:check` 均覆盖，共 7 处落点）。
- `npm run version:check` 校验 7 处一致（`package.json` / `tauri.conf.json` / `src-tauri/Cargo.toml` / `src-tauri/core/Cargo.toml` / `AGENTS.md` 三行）；推进版本一律用 `npm run version:set -- <x.y.z>`。
- **强制递增规则**：**每一次完成的修改落盘后，都必须按规则递增版本号**，无需逐轮请示——运行 `npm run version:set -- <x.y.z>` 同步 7 处落点、`npm run version:check` 通过，并在回复中如实报出新版本号与校验结果。档位按改动性质裁定：新功能或对既有功能同类范围的扩展 = minor（y+1）；缺陷修复（含同一自然日对同一模块/同一类 bug 的追加修复）= patch（z+1）。
- **唯一豁免（不递增）**：修复**上一轮因理解错误**造成的问题（同一问题的返工性重做）；此时版本号保持不变，并在回复中注明豁免原因。
- **以下情形仍不递增**（强制递增规则之外）：
  1. 用户明确要求"不修改版本号 / 回退到 X.Y.Z"（用户指令永远优先）；
  2. 纯 `docs/`、`README.md`、本 `AGENTS.md` 等不触碰代码行为的文档类改动——默认不递增，用户明确要求时从其指令；
  3. 纯文案、注释、日志措辞、去抖等不引入新逻辑分支的打磨——同上；
  4. 移植/迁移过程中的等价实现（行为未变即版本未变），除非用户对该次改动明确指示递增。
- 用户要求"回退到 X.Y.Z"时，所有落点必须一致改写为用户指定值，本回合内不得再以"我刚改了代码所以 +1"为由推进。
- **`1.2.x ~ 1.3.x` 段位：2026-10-13 由维护者本人改判，此前的「作废、不得再占用」不再适用**。改判依据是可核对的事实：该段位从未构建、从未打标签（改判当时 `git tag` 最高只到 `v1.1.15`，`v1.2.0` 尚未创建），所以把基准版本取到 **1.2.0** 不与任何已发布产物或已推送标签冲突。`v1.2.0` 已于 2026-10-14 打标签发布，后续按上面的强制递增规则从 1.2.x 继续推进；曾写到的 `1.4.x` / `1.5.x` 属同一批从未构建、从未打标签的值，随本次改判一并弃用，**不得**再回到那两个段位。

## 变更与交付约定（面向 Agent）

1. **改动前**：`read` 读取目标文件原文；涉及 `orchestration.rs` 编排、`server.rs` 路由、`protocol.rs` 校验、`runtime.rs` 下载的逻辑，必须同时阅读相关测试（含 `tests/fixtures/*.json` 的快照断言）确认既有行为边界。
2. **改动中**：一次只改一个独立区块/函数，避免大范围替换；不修改与任务无关的文件。
3. **改动后**：
   - 必须运行 `src-tauri/core/` 的 `cargo test` 与 `cargo clippy --all-targets`，并在回复中给出**真实的**通过/失败与 warning 数量；
   - 改动 `src/**`（面板）或 `scripts/*.mjs` 后必须运行 `npm run test:prefs`、`npm run test:ops`、`npm run test:ai`、`npm run test:manifest` 与 `npm run test:updater-key`、`npx eslint .` 与 `npm run vite:build`，同样**如实报数**；改壳（`src-tauri/src/`）后补 `cargo test --lib`（`src-tauri/`）；改 `release.yml` 后必须 `python3 -c "import yaml;yaml.safe_load(open('.github/workflows/release.yml'))"` 并复述各作业步骤名；改 `scripts/make-dmg.sh` 后必须 `bash -n` 它；
   - 必须用 `grep` / `read` 验证改动已落盘；
   - **版本步骤（强制）**：改动落盘并验证后，按「版本号规则」执行 `npm run version:set -- <x.y.z>` 递增并 `npm run version:check` 确认 7 处一致，在回复中报出新版本号；仅返工豁免（修复上一轮理解错误）与纯文档类改动不递增；
   - 不得自动 `git commit` / `git push`；
   - 若改动涉及对外契约（路由、错误码、`status.json` 字段、`models.json` 写入格式、IPC 命令/事件），必须在回复中显式列出并提示用户影响面；
   - 若改动触及红线，必须同步更新 `tests/red_lines.rs` 的对应断言。
4. **遇到不确定**：宁可向用户询问，也不要凭推测修改安全、供应链、隔离相关代码。
