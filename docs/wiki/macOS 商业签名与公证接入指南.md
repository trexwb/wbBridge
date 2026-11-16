# macOS 商业签名与公证接入指南

> **状态：尚未接入**（截至 2026-10-12）。本页是「Apple Developer Program 续费之后照着执行」的接入手册，不是已完成记录。
>
> 当前所有 macOS 产物仍是 **ad-hoc 签名**（`src-tauri/tauri.conf.json` → `bundle.macOS.signingIdentity: "-"`），构建期固定打一条 `Warn skipping app notarization, no APPLE_ID & …`（属预期，见[版本与发布](版本与发布)）。代价是 `spctl` 判定 rejected，用户首次打开需按[快速开始](快速开始)「macOS 首次打开」放行。

**一句话路径**：续费会员 → 本机生成 CSR → 平台签发 Developer ID Application 证书 → 与本机钥匙串私钥配对 → 导出 `.p12` → 变量进 CI → 改 `tauri.conf.json` → 补公证凭据 → 逐产物 `codesign` / `spctl` / `stapler` 验证。

**改动面**：不动 Rust / Vue 代码。预计只改 `src-tauri/tauri.conf.json`、`.github/workflows/release.yml`、`scripts/make-dmg.sh`，并新增 `src-tauri/entitlements.plist`。

---

## 1. 前置条件

| # | 前置条件 | 当前是否满足 | 说明与取证 |
|---|---|---|---|
| 1 | **Apple Developer Program 会员有效** | ❌ **已到期** | 阻塞根因。个人/组织年费 99 USD；组织账号续订须由「账号持有人」完成，可在到期前 30 天内手动续订，也可在到期后补续 |
| 2 | **可存取 Certificates, Identifiers & Profiles** | ❌ **不可用** | 会员到期即失去该入口（Apple 官方帮助：到期后 `You lose access to Certificates, Identifiers & Profiles`），因此**无法下载也无法创建** Developer ID 证书 |
| 3 | **本机登录钥匙串持有与该证书配对的私钥** | ❌ **没有** | 2026-10-12 实读 `security find-identity -v -p codesigning` → `0 valid identities found`。⚠ 私钥**必须**是生成 CSR 那台机器上的那一把，事后无法补配；`security import` 只搬证书（公钥）时该身份**不会**被计入 valid identities |
| 4 | **Team ID** | ⚠ **待取得** | 续费后于 developer.apple.com → Account → **Membership details** 读取（10 位字母数字）。本仓库**尚未记录**该值，配置与 CI 变量里都不得凭猜测填写 |
| 5 | 构建机具备签名/公证工具链 | ✅ 本机具备 | macOS 27.0.1（build 26A434）、`xcrun version 72`、`xcrun notarytool 1.1.3 (42)`；GitHub 的 `macos-14` / `macos-15` runner 自带 Xcode 工具链 |
| 6 | Tauri CLI 支持签名与公证参数 | ✅ | `@tauri-apps/cli` 2.12.1（`bundle.macOS` 已含 `signingIdentity` / `hardenedRuntime` / `entitlements` / `providerShortName`） |
| 7 | 仓库 `dev` 分支工作区干净 | ✅ | 2026-10-12 `git status --porcelain` 无输出 |

> ⚠ **第 3 条是最容易踩空的一条**：`.cer` 文件里只有公钥。若私钥不在本机（换过电脑、清了钥匙串、只在另一台机器上生成过 CSR），导入 `.cer` 后 `security find-identity -v -p codesigning` 仍然是 0 —— 这时唯一出路是**在本机重新生成 CSR、重新签发一张证书**，不是「想办法把 .cer 装进去」。

### 建议的前置小改（尚未执行）

`.gitignore` 目前**没有**覆盖证书类文件（已实读确认），建议在动证书之前补上，避免误提交：

```
# 签名证书与私钥（严禁提交）
*.p12
*.cer
*.certSigningRequest
*.mobileprovision
```

---

## 2. 当前状态与阻塞

### 2.1 阻塞链

```
Apple Developer Program 会员过期
  └─► 失去 Certificates, Identifiers & Profiles 访问权
        └─► 无法创建 / 下载 Developer ID Application 证书
              └─► 本机钥匙串里没有可用的签名身份（0 valid identities）
                    └─► bundle.macOS.signingIdentity 只能维持 "-"（ad-hoc）
                          └─► 无 Developer ID 证书 ⇒ 也无法做 notarytool 公证
                                └─► 产物未公证 ⇒ spctl 判定 rejected，用户需手动放行
```

**关键结论：整条链上只有一个可移除的致因 —— 会员资格。** 续费完成前，第 3 节里的任何一步都无法推进。

### 2.2 证据（2026-10-12 实读）

| 检查 | 命令 | 结果 |
|---|---|---|
| 代码签名身份 | `security find-identity -v -p codesigning` | `0 valid identities found` |
| 全部用途的签名身份 | `security find-identity -v` | `0 valid identities found`（排除「用途不匹配」的可能，本机确实没有任何签名身份） |
| Developer ID 证书 | `security find-certificate -a -c "Developer ID Application" -Z` | 无输出（登录钥匙串里没有任何 Developer ID 证书） |
| 配置现状 | `src-tauri/tauri.conf.json` | `bundle.macOS = {"minimumSystemVersion":"10.15","signingIdentity":"-"}`；无 `hardenedRuntime` / `entitlements` 显式项；`bundle.targets` 为 `["app","nsis","msi","appimage","deb"]`（**不含 dmg**）；`bundle.createUpdaterArtifacts: true` |

### 2.3 会员到期会发生 / 不会发生什么（Apple 官方口径）

| 会 | 不会 |
|---|---|
| 无法提交新 App 或更新 | **已安装的应用仍可继续使用**（本地已装的 `WB Bridge.app` 不受影响，只是无法再收到新版本） |
| 失去 Certificates, Identifiers & Profiles 访问权 ⇒ 无法签发新证书 | 不会使**已经签出**的产物失效 —— Developer ID 签名的应用运行时不联网校验会员状态 |
| 无法做公证（notarytool 需要有效会员） | 不会删除你的开发者账号或已发布的历史资产 |
| 免费 App 在续费后约 24 小时内恢复上架状态 | 无需重新提交已有 App |

**续订窗口**：到期前 30 天内可手动续订；**到期后一年之内可随时重新订阅**。因此本次阻塞**没有时间紧迫性**，但拖得越久，本机钥匙串被清理 / 换机的概率越高（届时需重签证书，见 §1 第 3 条）。

---

## 3. 接入步骤

> 步骤 0–5 在**用户本机**完成（涉及私钥，不得在 CI 上做）；步骤 6–10 是仓库改动。

### 步骤 0：续费并解锁平台入口

1. developer.apple.com → 右上角账号图标 → 登录（用户已在 Chrome 登录）。
2. Account → **Membership details** → 确认状态由 Expired 变为 Active，**记录 Team ID**。
3. 复查 Certificates, Identifiers & Profiles 是否可进入（这是本步骤的验收条件，不是「付款成功」）。
4. 顺手建议：对已到期/不再使用的旧证书（Development、Mac Installer Distribution 等）做一次清理，避免后续「同名多身份」干扰 `security find-identity` 的判读。

### 步骤 1：本机生成 CSR

**推荐走钥匙串访问 GUI**（私钥由系统自动落到登录钥匙串，天然完成配对）：

1. 「钥匙串访问」→ 菜单 **钥匙串访问 → 证书助理 → 从证书颁发机构请求证书…**
2. 「用户电子邮件地址」填 Apple ID 邮箱；「常用名称」填可辨识名（如 `WB Bridge Developer ID`）；**勾选「存储到磁盘」**，同时勾选「**让我指定密钥对信息**」→ 继续 → 密钥大小 **2048**、算法 **RSA** → 继续，存成 `WBBridge.certSigningRequest`。

命令行等价写法（私钥落在当前目录，**之后必须手动导入登录钥匙串**，多一步易错）：

```sh
openssl req -new -newkey rsa:2048 -nodes \
  -keyout ~/WBBridge-DeveloperID.key \
  -out ~/WBBridge.certSigningRequest \
  -subj "/emailAddress=<Apple ID 邮箱>/CN=WB Bridge Developer ID"
```

⚠ **CSR 不是凭据，但私钥是**：`WBBridge-DeveloperID.key` / 导出后的 `.p12` 一律不入库、不进日志、不进聊天记录。

### 步骤 2：平台创建 Developer ID Application 证书并下载 `.cer`

1. developer.apple.com → Account → **Certificates, Identifiers & Profiles** → **Certificates** → **+**。
2. 选 **Software → Developer ID Application** → 继续（用途：分发到 App Store 之外，正是本项目的场景；`Developer ID Installer` 用于 `.pkg` 安装器，本项目不用）。
3. 上传步骤 1 的 `.certSigningRequest` → **Download**，得到 `developerID_application.cer`。
4. 记下证书页显示的名称与 **Expiration**（`Developer ID Application: <名称> (<Team ID>)` —— 括号里的 Team ID 必须与步骤 0 记录的一致）。

### 步骤 3：导入 `.cer`，与私钥配对

1. 双击 `developerID_application.cer` → 加入**「登录」钥匙串**。
2. 打开「钥匙串访问 → 登录 → 我的证书」，找到 `Developer ID Application: …`，**展开左侧箭头应能看到配对私钥**（看不到即私钥缺失，回到 §1 第 3 条）。
3. 右键该证书 → 显示简介 → 展开「信任」，确认 `代码签名` 一栏不是「永不信任」。

### 步骤 4：验证签名身份（本步骤是「配对是否成功」的唯一判据）

```sh
security find-identity -v -p codesigning
```

**期望**：`1 valid identities found`，并列出 `"Developer ID Application: <名称> (<Team ID>)"`。

同时读一下证书真实有效期，后续排查「签名突然失败」时要靠它：

```sh
security find-certificate -a -c "Developer ID Application" -p \
  | openssl x509 -noout -subject -enddate
```

### 步骤 5：导出 `.p12` 供 CI

**推荐 GUI**（可精确只导这一条身份）：钥匙串访问 → 登录 → 我的证书 → 右键 `Developer ID Application: …` → **导出…** → 文件格式选 **个人信息交换 (.p12)** → 设一个导出密码 → 存成 `WBBridge-DeveloperID.p12`。

命令行等价写法（⚠ `-t identities` 会把钥匙串里**所有**身份一并导出，本机若有多个证书会污染产物）：

```sh
security export -k ~/Library/Keychains/login.keychain-db \
  -t identities -f pkcs12 \
  -P "<导出密码>" -o ~/WBBridge-DeveloperID.p12
```

转成 base64 供 CI 使用（macOS 的 `base64` 用 `-i/-o`）：

```sh
base64 -i ~/WBBridge-DeveloperID.p12 -o ~/WBBridge-DeveloperID.p12.b64
```

验证 base64 可逆（避免粘进 CI 时被换行/截断破坏）：

```sh
base64 -d -i ~/WBBridge-DeveloperID.p12.b64 -o /tmp/roundtrip.p12 \
  && cmp ~/WBBridge-DeveloperID.p12 /tmp/roundtrip.p12 && echo "OK"
```

### 步骤 6：配置 GitHub 变量

⚠ **不要把证书凭据放进现有那套 `vars.`**。仓库当前把 *updater 的 minisign 私钥*放在 **Repository Variables**（`vars.TAURI_SIGNING_PRIVATE_KEY{,_PASSWORD}`），那是既有决定、且有明确记录（Variables 明文可见、日志不打码）。**Developer ID 证书是真正的分发凭据，必须走 Repository Secrets。**

需要新增的 Secrets：

| Secret | 内容 | 来源 |
|---|---|---|
| `APPLE_CERTIFICATE` | `.p12` 的 base64 **全文** | 步骤 5 的 `.p12.b64` |
| `APPLE_CERTIFICATE_PASSWORD` | `.p12` 的导出密码 | 步骤 5 自设 |
| `KEYCHAIN_PASSWORD` | CI 上临时钥匙串的密码（**任意新造**，非 Apple 凭据） | 自行生成随机串 |
| `APPLE_SIGNING_IDENTITY` | `Developer ID Application: <名称> (<Team ID>)` | 步骤 2 证书页 |
| `APPLE_TEAM_ID` | 10 位 Team ID | 步骤 0 |
| `APPLE_ID` | Apple ID 邮箱 | — |
| `APPLE_PASSWORD` | **App 专用密码**（app-specific password，非账号登录密码） | appleid.apple.com 生成 |
| `APPLE_API_KEY` / `APPLE_API_ISSUER` / `APPLE_API_KEY_PATH` | 仅走 **App Store Connect API Key** 路线时才需要（`.p8` 文件内容 + Issuer ID + 落地路径） | App Store Connect → Users and Access → Integrations |

> **公证凭据二选一即可**：Apple ID 路线（`APPLE_ID` + `APPLE_PASSWORD` + `APPLE_TEAM_ID`）更简单；API Key 路线更适合团队/自动化。二者不必都配。
>
> ⚠ **凭据一律不得索取、不得代填、不得猜测**：App 专用密码、Issuer ID、`.p8` 都必须由用户在对应平台自行生成后填入 Secrets；证书导出密码同理。

在 `.github/workflows/release.yml` 里**只给 `build-macos` 作业**加变量（Windows / Linux 作业不加；现有 `test`、`update-manifest` 作业也不加）。落点是该作业「构建 Tauri 应用」步骤的 `env:` 块（现为 `TAURI_SIGNING_PRIVATE_KEY` / `..._PASSWORD`，约第 225–228 行）：

```yaml
        env:
          TAURI_SIGNING_PRIVATE_KEY: ${{ vars.TAURI_SIGNING_PRIVATE_KEY }}
          TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ vars.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}
          # 以下为 macOS 商业签名 / 公证新增（走 Secrets，不走 vars.）
          APPLE_CERTIFICATE: ${{ secrets.APPLE_CERTIFICATE }}
          APPLE_CERTIFICATE_PASSWORD: ${{ secrets.APPLE_CERTIFICATE_PASSWORD }}
          KEYCHAIN_PASSWORD: ${{ secrets.KEYCHAIN_PASSWORD }}
          APPLE_SIGNING_IDENTITY: ${{ secrets.APPLE_SIGNING_IDENTITY }}
          APPLE_ID: ${{ secrets.APPLE_ID }}
          APPLE_PASSWORD: ${{ secrets.APPLE_PASSWORD }}
          APPLE_TEAM_ID: ${{ secrets.APPLE_TEAM_ID }}
```

⚠ 注意 macOS 作业是**两个 runner**（`macos-14` ARM 与 `macos-15` Intel，见 `matrix.arch`），签名变量对两者都生效；`.dmg` 公证步骤（步骤 10）也必须加在同一作业里，否则只有一半平台产物被公证。

### 步骤 7：修改 `tauri.conf.json`

`src-tauri/tauri.conf.json` 是严格 JSON（**不能写注释**），当前 `bundle.macOS` 为：

```json
    "macOS": {
      "minimumSystemVersion": "10.15",
      "signingIdentity": "-"
    },
```

改为：

```json
    "macOS": {
      "minimumSystemVersion": "10.15",
      "signingIdentity": "Developer ID Application: <名称> (<Team ID>)",
      "hardenedRuntime": true,
      "entitlements": "entitlements.plist"
    },
```

要点：

- `signingIdentity` 必须与证书**显示名逐字一致**（含 `Developer ID Application: ` 前缀与括号里的 Team ID）。
- 若 CI 同时提供 `APPLE_CERTIFICATE`，Tauri 会**从该变量自动推断**签名身份；实读 `@tauri-apps/cli` 的 CHANGELOG（commit `8deb1966a`，PR #10652）：「导入的证书名与配置里的 `signingIdentity` 不匹配时会直接报错」。因此**保留配置项就必须写全名**，否则宁可删掉该键让自动推断接管，不要两边不一致。
- `hardenedRuntime` 在 Tauri 2 配置 schema 中**默认即为 `true`**；这里显式写出是为了让配置自解释、并避免默认值随 CLI 版本漂移。**公证要求 hardened runtime 开启**，不要改成 `false`。
- `entitlements` 指向相对 `tauri.conf.json` 所在目录（即 `src-tauri/`）的路径。见步骤 8。
- 组织账号若名下多团队、公证报 provider 不匹配，可再补 `providerShortName`（Team 的短名，非 Team ID）。

### 步骤 8：补 entitlements 文件

新建 `src-tauri/entitlements.plist`，**先取最小集**：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
</dict>
</plist>
```

⚠ 本项目启用了 hardened runtime 后，**必须实测**以下两条链路是否仍工作，并按公证问题报告逐条增补权限（不要预先堆砌）：

1. **子进程**：WB Bridge 会下载并以子进程方式启动隔离的 OpenCode 运行时 —— hardened runtime 约束的是主程序自身，被 spawn 的子进程不受其 entitlements 管辖；但若核心改用 `dlopen` 加载未签名动态库，则需要 `com.apple.security.cs.disable-library-validation`。
2. **更新器替换自身**：应用内自动更新会替换 `.app` 内容，若后续报权限错误，再评估 `com.apple.security.cs.allow-unsigned-executable-memory` 等（**默认不加**）。

取舍原则：entitlements 每加一条都扩大攻击面，且要重新公证；只在验证清单（§4）出现具体失败时加，并写明加了哪条、为什么。

### 步骤 9：接入公证（notarytool）

Tauri 在检测到公证凭据（`APPLE_ID` + `APPLE_PASSWORD` + `APPLE_TEAM_ID`，或 `APPLE_API_*`）时会**自动对 bundle 做公证**——构建期那条 `Warn skipping app notarization, …` 的存在本身就是在提示这件事。

因此步骤 6 配好凭据后：

- **该告警应消失**，这是「公证已接线」的第一手判据；告警仍在说明凭据没被读到（注意变量名、以及是否加在了正确的作业上）。
- ⚠ **覆盖面必须实测确认**：Tauri 的自动公证作用于它自己产出的 bundle（`.app` / `.app.tar.gz` 更新包），对 `.app` 是否执行了 `stapler`（离线票据）也要人工核。**`scripts/make-dmg.sh` 另打的 `.dmg` 完全不在 tauri bundle 流程内**（`bundle.targets` 显式不含 dmg），需要步骤 10 单独处理。

### 步骤 10：`.dmg` 单独公证并装订票据

这是本项目最容易漏的一步：`.dmg` 由 `npm run make:dmg`（`scripts/make-dmg.sh`，hdiutil）在 `tauri build` **之后**生成，因此不会被 tauri 的自动公证覆盖。用户从 Release 下载的正是 `.dmg` —— 不处理它，等于没解决问题。

在 `make:dmg` 之后加两步（在 `build-macos` 作业内，且两者都要带 Team ID）：

```sh
# 1) 提交公证（--wait 阻塞到出结果）
xcrun notarytool submit "$DMG" \
  --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID" \
  --wait

# 2) 把公证票据装订进 .dmg（离线也能被 Gatekeeper 验）
xcrun stapler staple "$DMG"
xcrun stapler validate "$DMG"
```

说明与注意：

- 若走 API Key 路线，改用 `--key <path-to-AuthKey.p8> --key-id <API_KEY_ID> --issuer <ISSUER_ID>`。
- 公证**耗时数分钟**，`--wait` 期间可能触发 runner 超时；必要时提高该步骤 `timeout-minutes`。失败时用 `xcrun notarytool log <submission-id>` 拿问题报告，再回到步骤 8 增补 entitlements。
- 顺序要求：**先签名 → 再公证 → 再 staple → 最后打包 `.dmg` → 对 `.dmg` 再次公证 + staple**。若把 `.dmg` 打在前面，`.app` 的票据会缺失，用户挂载后运行仍被拦。
- 公证产物一旦替换，`latest.json` 的 `.sig`（minisign，见[版本与发布](版本与发布)「签名与自动更新」）**不受影响** —— 两套签名彼此独立，但**新增步骤可能改变产物字节**，若更新包在 staple 后被重新打包，必须重新生成 `.sig`，否则客户端验签失败。

---

## 4. 可执行验证清单与回滚

### 4.1 本机（步骤 0–5 完成后）

| # | 检查项 | 命令 | 期望 | 当前（2026-10-12） |
|---|---|---|---|---|
| 1 | 签名身份存在且配对成功 | `security find-identity -v -p codesigning` | `1 valid identities found`，列出 `Developer ID Application: <名称> (<Team ID>)` | ❌ `0 valid identities found` |
| 2 | 证书真实有效期 | `security find-certificate -a -c "Developer ID Application" -p \| openssl x509 -noout -subject -enddate` | 有 subject 与 `notAfter` 输出 | ❌ 无任何 Developer ID 证书 |
| 3 | `.p12` 可导入 | 在临时钥匙串执行 `security import ~/WBBridge-DeveloperID.p12 -k <tmp.keychain> -P <密码>` 后 `security find-identity -v -p codesigning` | 临时钥匙串里能看到同一身份 | 未测（无 `.p12`） |
| 4 | base64 可逆 | `base64 -d -i x.p12.b64 -o y.p12 && cmp x.p12 y.p12` | 无输出（完全一致） | 未测 |

### 4.2 构建后（步骤 7–10 完成后，本机 `npm run build` 一次即可全量取证）

| # | 检查项 | 命令 | 期望 |
|---|---|---|---|
| 5 | 签名主体 | `codesign -dv --verbose=4 "src-tauri/target/release/bundle/macos/WB Bridge.app"` | `Authority=Developer ID Application: <名称> (<Team ID>)` |
| 6 | hardened runtime 已开 | 同上输出中查 `flags` | 含 `runtime`（形如 `flags=0x10000(runtime)`） |
| 7 | 深度签名校验 | `codesign --verify --deep --strict --verbose=2 "<app>"` | 退出码 0，`valid on disk` / `satisfies its Designated Requirement` |
| 8 | 公证票据已装订 | `xcrun stapler validate "<app>"` | `The validate action worked!` |
| 9 | Gatekeeper 判定 | `spctl -a -vvv -t install "<app>"` | `accepted` + `source=Notarized Developer ID`（当前为 `rejected`） |
| 10 | `.dmg` 同样通过 | `xcrun stapler validate "<...>.dmg"` 与 `spctl -a -vvv -t open --context context:primary-signature "<...>.dmg"` | 票据有效、`accepted` |
| 11 | 构建期告警消失 | 构建日志检索 `skipping app notarization` | **无匹配**（仍然出现 ⇒ 凭据没读到，回步骤 6/9） |
| 12 | 更新包签名未被打断 | 解 `.sig` 取签名者 key ID | 仍 = `2B11F78BEA8A43F`（与 `plugins.updater.pubkey` 唯一那条一致） |
| 13 | 首次打开无拦截 | 在一台**未装过**的 Mac 上从 `.dmg` 安装并直接双击 | 系统对话框为普通确认，**不再出现**「已损坏」；无需 `xattr -dr com.apple.quarantine` |

### 4.3 CI 首次实跑

- [ ] 推一个测试标签（或在 `workflow_dispatch` 勾选 `create_release`）后，`build-macos` 两个架构作业均 success。
- [ ] 构建日志里没有 `skipping app notarization`；有公证提交/装订步骤的执行记录。
- [ ] Release 资产数与既有轮次一致（23 个），资产名不受影响。
- [ ] 下载 macOS `.dmg` 与 `.app.tar.gz`，在本地机器上重复 §4.2 的第 5–10 项。
- [ ] `latest.json` 六条 `url` 逐条 200（该对账步骤已在 CI 内，实跑 success 即可）。

### 4.4 回滚

回滚的**目的是立刻恢复可发布状态**，不是撤销平台侧动作。顺序：先回配置与 CI，再处理凭据。

| 失败点 | 回滚动作 | 影响 |
|---|---|---|
| 签名后构建失败 / 产物无法启动 | `tauri.conf.json` 的 `signingIdentity` 改回 `"-"`，删 `hardenedRuntime` 与 `entitlements` 两键 → 提交 | 回到 ad-hoc 签名，构建期 `skipping app notarization` 告警按预期回归；用户需继续手动放行 |
| 仅公证失败（签名成功） | 从 `build-macos` 的 `env` 移除 `APPLE_ID` / `APPLE_PASSWORD` / `APPLE_TEAM_ID`（或 `APPLE_API_*`），保留签名变量 | 产物已签名未公证 ⇒ `spctl` 仍可能 rejected，但「已损坏」类提示会消失 |
| CI 钥匙串相关报错 | 从 `env` 移除 `APPLE_CERTIFICATE` / `APPLE_CERTIFICATE_PASSWORD` / `KEYCHAIN_PASSWORD` / `APPLE_SIGNING_IDENTITY` | 完全回到接入前状态，**无需重发版本** |
| `.dmg` 公证步骤阻断发布 | 删掉步骤 10 的两条命令（`.app` 仍已公证） | `.dmg` 用户仍可能被拦，但更新器侧的 `.app.tar.gz` 是公证过的 |
| 证书泄露 / 误提交 | 平台侧 **Revoke** 该证书 → 重新生成 CSR 并签发新证书 → 重新导出 `.p12` 并更新 Secrets | 已发布产物不受影响；新版本用新证书签。Revoke 前先确认 CI 不再使用旧证书 |

回滚后需要同步落盘的记录：

- [ ] `docs/wiki/版本与发布.md`「商业签名与公证」表的状态列改回 ❌ 并注明回滚原因与日期。
- [ ] `docs/wiki/已知限制与未验证项.md` 第 4 条「代码签名 / 公证」的状态与说明。
- [ ] `docs/wiki/Home.md`「当前状态一览」的「代码签名 / 公证」行。
- [ ] Release 说明里明示当前产物是否已公证（用户侧放行方式随之不同）。

---

## 5. 已知约束与注意事项

### 5.1 硬约束

| # | 约束 | 后果 / 应对 |
|---|---|---|
| 1 | **`.cer` 只含公钥，不能单独签名** | 证书必须与生成 CSR 时那把私钥配对；私钥丢失即证书作废，只能重新签发 |
| 2 | **私钥必须在构建机上** | 本机：登录钥匙串；CI：由 `APPLE_CERTIFICATE`（base64 `.p12`）+ `KEYCHAIN_PASSWORD` 导入 runner 的临时钥匙串。签名不可能「代理到云端」 |
| 3 | **会员到期后无法签发新证书、无法公证** | 已签出的产物继续可用，但不能再签新版本；公证是线上服务，凭据有效也无法在会员过期时提交 |
| 4 | **到期后一年内可续订** | 官方口径：到期前 30 天内可手动续订，到期后一年内可随时重新订阅。超过一年未续订，账号/资源可能被回收，恢复成本显著上升 |
| 5 | **换机 = 重签证书** | 私钥不可迁移就只剩重签一条路；接入后应把 `.p12` 与导出密码备份到离线介质/密码管理器 |
| 6 | **`.dmg` 不在 tauri 公证范围内** | 见步骤 10；只做步骤 9 会让「下载 `.dmg` 的用户」这条主路径仍然被拦 |
| 7 | **签名链与更新器 minisign 链相互独立** | Developer ID 证书 ≠ `TAURI_SIGNING_PRIVATE_KEY`；前者管 Gatekeeper，后者管自动更新验签。换任一方都不要顺手动另一方 |

### 5.2 注意事项

- **凭据纪律**：`.p12`、导出密码、App 专用密码、`.p8` 一律不入库、不写进 `status.json`、不 echo 到 CI 日志。证书类凭据走 **Secrets**（自动打码），不要沿用仓库现有的 `vars.`（Variables 明文可见）。接入后请顺手确认 CI 里没有把新变量打进 `$GITHUB_STEP_SUMMARY`。
- **`.gitignore` 建议先补**（见 §1「建议的前置小改」）——当前 `*.p12` / `*.cer` / `*.certSigningRequest` 都不在忽略列表里。
- **配置不能写注释**：`tauri.conf.json` 是严格 JSON，所以「为什么这么配」只能写在本页与[版本与发布](版本与发布)里。
- **首轮只改 macOS**：Windows 尚无商用代码签名（见[版本与发布](版本与发布)），本次接入不要顺手改 Windows 配置，避免一次引入两个未验证变量。
- **公证耗时**：`notarytool submit --wait` 常见几分钟；`.dmg` 公证再叠加一次。发布流水线总时长会明显变长，超时阈值要一并调。
- **entitlements 只增不预堆**：每加一条都要重新公证并扩大攻击面；只在验证清单出现具体失败时增补，并记录原因。
- **序号/命名一致性**：`signingIdentity` 文本、`APPLE_SIGNING_IDENTITY`、证书页显示名三者必须逐字一致（Team ID 也要一致），不一致会在 CI 上报「证书名与 signingIdentity 不匹配」。
- **本仓库尚未实机走过任何商用签名**：本页的键名与环境变量名来自官方文档 `distribute/Sign/macos.mdx` 与 `@tauri-apps/cli` 2.12.1 的 schema / CHANGELOG（2026-10-12 实读），**接入当天仍要按当时官方文档复核一遍**。
- **不要用本页替代取证**：`spctl` 从 rejected 变 accepted、`stapler validate` 通过，才是「接入成功」的证据；构建退出码 0 不是。

---

## 6. 接入完成后需同步更新的文档

| 文档 | 需要改什么 |
|---|---|
| [版本与发布](版本与发布) | 「商业签名与公证」表 macOS 两行状态 ❌ → ✅，补实跑日期/run 号；「代码签名 / 公证」行同步；`tauri.conf.json` 的 `signingIdentity` 现值 |
| [快速开始](快速开始) | 「macOS 首次打开（ad-hoc 签名放行）」一节需改写为「已公证，正常打开」并保留历史说明供旧版本用户参考 |
| [常见问题与故障排查](常见问题与故障排查) | Gatekeeper 条目（现归因于 A7 未接入）需区分「≥ 接入版本」与「旧版本」两条路径 |
| [已知限制与未验证项](已知限制与未验证项) | 第 4 条「代码签名 / 公证」状态与说明 |
| [Home.md](Home) | 「当前状态一览」的「代码签名 / 公证」行 |
| `docs/plans/2026-10-11-next-phase-iteration-plan.md` | R-07（商用代码签名 / Apple 公证）关闭并标注取证；注意该计划 §5.1 曾把 A7 排除在范围外 |
| `AGENTS.md` | 若构建/发布命令有变（如新增 `make:dmg` 之后的公证步骤），命令清单需同步 |

---

## 取证记录

| 结论 | 依据 | 时间 |
|---|---|---|
| 本机 0 个有效签名身份、无 Developer ID 证书 | `security find-identity -v -p codesigning` / `-v` / `find-certificate -a -c "Developer ID Application" -Z` 实跑 | 2026-10-12 |
| 当前 macOS 配置为 ad-hoc | `src-tauri/tauri.conf.json` 实读（`bundle.macOS.signingIdentity: "-"`） | 2026-10-12 |
| 工具链可用 | `sw_vers`、`xcrun --version`、`xcrun notarytool --version` 实跑 | 2026-10-12 |
| `bundle.targets` 不含 dmg、`.dmg` 由脚本另打 | `src-tauri/tauri.conf.json` + `scripts/make-dmg.sh` 实读 | 2026-10-12 |
| `hardenedRuntime` 默认 `true`、`entitlements` 为路径 | `node_modules/@tauri-apps/cli/config.schema.json` 实读 | 2026-10-12 |
| `APPLE_CERTIFICATE` 会推断签名身份、名字不匹配即报错 | `node_modules/@tauri-apps/cli/CHANGELOG.md`（commit `8deb1966a`，PR #10652） | 2026-10-12 |
| 会员到期即失去 Certificates, Identifiers & Profiles 访问权；续订可至到期前 30 天 / 到期后一年内 | Apple Developer 官方帮助「计画续订」/「Resolving access issues」（`developer.apple.com/help/account/…`） | 2026-10-12 |

> 相关页面：[版本与发布](版本与发布)（发布流程、签名与自动更新、A7 预留位）、[快速开始](快速开始)（首次打开放行）、[开发指南](开发指南)（构建命令与签名注入包装器）、[已知限制与未验证项](已知限制与未验证项)。
*（内容由AI生成，仅供参考）*
