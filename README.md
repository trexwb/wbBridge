# WB Bridge

基于 Tauri 2 的跨平台托盘应用，通过隔离的 OpenCode 运行时为 [WorkBuddy](https://www.workbuddy.cn) 提供免费模型（CodeBuddy 同样支持）。当前版本 **1.1.15**。

## 简介

WB Bridge 做三件事：

1. **托管一个隔离的 OpenCode 运行时**：优先复用本机已有的 `opencode`，否则从 npm registry（官方源 → 国内镜像）下载官方 tarball，校验 `sha512-` 完整性后解包到数据目录，用隔离环境变量 + 独立随机端口以 `serve --pure` 启动，不污染用户已有 OpenCode 的配置、数据、缓存与登录态。
2. **在本地提供 OpenAI 兼容接口**：`127.0.0.1:<port>` 上的 `/v1/models` 与 `/v1/chat/completions`（含 SSE 流式）；所有路由都要 Bearer 鉴权（值为数据目录 `api-key` 文件的内容），任何非空 `Origin` 一律 403，并发 ≤8、请求体 ≤8MB。
3. **把可用模型同步给插件**：自动发现免费模型 → 逐模型真实探测（会消耗少量免费额度）→ 只把探测通过的模型写进 WorkBuddy / CodeBuddy 的 `models.json`（原子替换 + 增量合并，只动本应用拥有的条目，不产生 `.bak` 备份；退出时清理自己名下的条目并保留用户手动配置）。

其他要点：

- **可选接入自己的平台 Key**：侧栏「平台」视图填入 ModelScope（魔搭）/ SiliconFlow / 腾讯混元 TokenHub / 智谱 任一家 Key，该家的免费模型会和 OpenCode 自带的免费模型一起被发现并发布。Key 只落盘到数据目录 `providers.json`（`0600`），面板只显示「已配置 / 未配置」，绝不进日志、`status.json`、面板偏好或子进程环境变量。
- **`WB · auto`：不用挑模型**：把 `model` 填成 `WB · auto`，核心就在**当前可用**的免费模型里按请求内容选一个真实模型（带图片只挑支持视觉的；带工具定义避开「仅对话」且要求支持函数调用；同级内随机分散）。响应里的 `model` 是**实际模型的全限定 id**，用量与逐模型状态都记在它身上；该模型真实报错时按既有判决被移出可用集合，恢复要靠面板「重新检测」或「刷新」。可用池为空时这个名字不出现在列表里。模型列表非空时，它会作为独立条目写进 WorkBuddy / CodeBuddy 的 `models.json`，在插件模型选择器里可直接选中；`同步数量` 仍只数真实模型、不含这一条。详见[使用指南](docs/wiki/使用指南.md)与[核心概念与功能](docs/wiki/核心概念与功能.md)。
- **关闭窗口即退出应用**（macOS / Windows / Linux 一致）：走与托盘「退出」相同的优雅关停链（停核心 → 清理插件配置 → 结束进程），不驻留托盘、无需二次退出；托盘在运行期间可唤回面板、切代理、重选配置、退出。核心是静态链接进壳的 Rust 库，生命周期由壳掌握；单独运行核心可执行文件时，`BUDDY_PARENT_PID` 父进程看门狗会在父进程消失后自行优雅退出。
- 界面：左侧栏为分组导航（模型 / 运行 / 集成 / 其他 + 运行设置），模型详情是**右侧常驻分栏**（`Esc`、详情头部「收起详情」或窗口失焦收起）。详见[使用指南](docs/wiki/使用指南.md)。

## 下载与安装

推送 `v*` 标签后 GitHub Actions 自动构建六平台（macOS ARM/Intel、Windows x64/ARM、Linux x64/ARM）安装包并发布 Release（每轮 23 个资产：六平台安装包 + 各自 `.sig` + `latest.json`）。

| 系统 | 安装包 |
|---|---|
| macOS 10.15+（Apple Silicon / Intel） | `WB.Bridge_<版本>_aarch64.dmg` / `…_x64.dmg`（hdiutil 生成）；另有 `WB.Bridge_<arch>.app.tar.gz`（+ 同名 `.sig`）供自动更新 |
| Windows 10/11 x64 / ARM64 | `WB.Bridge_<版本>_x64-setup.exe` / `…_arm64-setup.exe`（+ 同名 `.sig`；另有 `.msi`，不参与更新） |
| Linux x64 / ARM64 | `WB.Bridge_<版本>_amd64.AppImage` / `.deb`（`.sig` 齐全；更新包就是裸 `.AppImage`） |

> 安装包名里的空格在 GitHub 侧会被规范化成 `.`：磁盘上是 `WB Bridge_…`，页面上与下载链接里是 `WB.Bridge_…`。

### macOS 首次打开

应用使用 ad-hoc 签名（未做 Apple 公证）。首次打开若被拦截：先尝试打开，再到「系统设置 → 隐私与安全性」点击「仍要打开」；若提示「已损坏」，确认来源可信后执行：

```sh
xattr -dr com.apple.quarantine "/Applications/WB Bridge.app"
```

商用签名与公证的接入步骤见 [macOS 商业签名与公证接入指南](docs/wiki/macOS%20商业签名与公证接入指南.md)。

### 首次使用

1. 先安装并登录 WorkBuddy（CodeBuddy 同样支持，两个插件可以都装）。在插件里保存过一个自定义模型最稳妥——它会生成 `models.json`；插件目录已存在但还没有这个文件时，本应用会补建一个空配置再照常发布（已存在的文件绝不改写，插件目录不存在也不会替插件建目录）。
2. 启动 WB Bridge：自动准备运行时、扫描并检测免费模型，找到有效配置后自动导入。
3. 找不到配置时点「导入」选择 `models.json`；Windows 托盘菜单「选择 WorkBuddy 配置…」可更换位置。
4. 「使用系统代理」开关控制运行时下载与模型请求是否走系统 HTTP/HTTPS 代理。

在任何 OpenAI 兼容客户端里用 `WB · auto` 接入（Base URL `http://127.0.0.1:41980/v1`，端口以面板 / `status.json` 为准）：

```sh
curl -s http://127.0.0.1:41980/v1/chat/completions \
  -H "Authorization: Bearer $(cat "$HOME/Library/Application Support/app.wbbridge.desktop/api-key")" \
  -H "Content-Type: application/json" \
  -d '{"model":"WB · auto","messages":[{"role":"user","content":"你好"}]}'
```

先用 `GET /v1/models`（同样要带 Bearer）确认它在列表末位，再发请求。命令行与桌面 agent 直连可用，**浏览器页面里 `fetch` 不行**（任何非空 `Origin` 一律 403）。

### 从源码构建

依赖：Rust stable（壳与核心均 `rust-version = "1.90"`，下限由锁定的依赖图决定）、Node.js **24+**（只用于 Vite 构建面板与仓库根的 `scripts/*.mjs`，核心运行不依赖它）、各平台 Tauri 系统依赖（Linux 需 `libwebkit2gtk-4.1-dev` 等）。

```sh
npm install
npm test                  # 核心测试（cargo test --manifest-path src-tauri/core/Cargo.toml）
npm run lint              # eslint（面板）
npm run dev               # 开发运行（tauri dev）
npm run build             # 桌面应用构建（vite:build && tauri:build && make:dmg）
npm run version:check     # 校验 7 处版本号落点一致
```

构建需要 updater 签名私钥：`bundle.createUpdaterArtifacts` 已开启，`npm run build` 中的 `tauri:build` 经 `scripts/with-updater-key.mjs` 注入签名——按「进程环境 > `.env.local` / `.env` > `~/.tauri/wbBridge{,-updater}.env` > `~/.tauri/wbBridge-updater.key`」汇齐私钥与口令，逐项打印来源文件名，绝不回显密钥。自己签就生成一把（`npm run tauri -- signer generate -p '' -w ~/.tauri/my.key`）并把 `plugins.updater.pubkey` 换成自己的公钥（**只有配置里第一条公钥生效**，换钥必须同步换 pubkey，否则产物的 `.sig` 与配置公钥不配对、客户端验签必失败）；只是想出安装包不要更新链，可临时关掉 `createUpdaterArtifacts`。细节见[版本与发布](docs/wiki/版本与发布.md)。

核心也可脱离桌面壳单独运行（同一份编排代码）：

```sh
cargo run --manifest-path src-tauri/core/Cargo.toml
curl -H "Authorization: Bearer $(cat "$HOME/Library/Application Support/Buddy Bridge/api-key")" \
     http://127.0.0.1:41980/health
```

> 数据目录随运行形态不同，排查时不要混用：桌面应用使用 Tauri `app_data_dir`（macOS `~/Library/Application Support/app.wbbridge.desktop`）；独立核心二进制使用平台默认目录（macOS `~/Library/Application Support/Buddy Bridge`、Windows `%APPDATA%\Buddy Bridge`、Linux `${XDG_CONFIG_HOME:-~/.config}/Buddy Bridge`），可用 `BUDDY_DATA_DIR` 覆盖。`/health` 同样需要 Bearer 鉴权。

## 已知限制

- 免费模型名单、额度与可用性由上游 OpenCode 决定，本应用不控制也不缓存上游额度。
- 未做商用代码签名 / 公证：Windows 可能提示未知发布者，macOS 需按上方步骤放行。
- Windows ARM64 与 Linux 包由 CI 构建，尚未实机验证。
- 仅 PAC / SOCKS 代理暂不支持。
- 面板以只读展示为主：运行日志、用量与额度、插件集成、关于与更新四个视图自身不写文件、不访问 OpenCode；**唯一的联网动作是自动更新**，且发生在 Rust 侧（Tauri updater 插件），不经面板 `fetch`。
- 浅色主题下仍有 3 处次要文字（副标题、页脚说明、耗时行）沿用 `--muted`，对比度低于 WCAG AA 的 4.5:1。
- 「检查 → 下载 → 安装 → 重启生效」的自动更新完整链路尚未端到端走过；该链路对 v1.0.1 及更早版本不生效（那一版没接更新器，需手动下载一次安装包）。
- `cargo fmt --check` 未作为门禁，代码风格以 clippy 0 warning 为准。

更多说明见 [文档首页](docs/wiki/Home.md)：功能说明、使用指南、架构设计、开发指南、常见问题与故障排查、已知限制与未验证项。
