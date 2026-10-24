# 版本落点

当前统一版本 **1.2.4**，共 7 处落点：根 `package.json`（唯一来源）、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、`src-tauri/core/Cargo.toml`、`AGENTS.md` 基准表三行；一致性用 `npm run version:check` 校验，推进用 `npm run version:set -- <x.y.z>`（`status.json` 内写入的 `0.2.0` 是历史沿革值，不属版本落点）。历史版本的 Release 正文底本见 `docs/release/`（最近一份为 `docs/release/v1.2.4.md`），更早版本从 git 历史取回。
