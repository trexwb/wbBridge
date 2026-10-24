// WB Bridge Tauri 壳：进程内核心的生命周期、托盘、状态推送与 IPC 代理。
// 核心（wbbridge_core::orchestration）跑在壳内的专用 tokio 运行时上，仍在 127.0.0.1:<port>
// 提供同一套 HTTP 契约；本壳负责：装入 data_dir/port → 轮询 status.json 推送事件 → 退出时经
// POST /admin/shutdown 优雅停止（等待核心完成 WorkBuddy 配置清理与串行写链落盘）。
use std::collections::HashMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent};
use tauri_plugin_dialog::DialogExt;
use wbbridge_core::orchestration::{self, StartOptions};

/// 进程内运行的核心：专用多线程 tokio 运行时 + 监听端口。
/// 运行时必须与壳自己的异步上下文隔离，否则 `block_on` 会嵌套 panic；持有它即持有核心任务。
struct RunningCore {
    runtime: tokio::runtime::Runtime,
    port: u16,
}

struct AppState {
    core: Mutex<Option<RunningCore>>,
    data_dir: PathBuf,
    quitting: AtomicBool,
    /// 核心编排任务是否已结束（由核心注入的 exit hook 置位）。
    core_stopped: AtomicBool,
    /// 核心结束时的退出码；0 表示按用户意愿停止。
    core_code: AtomicU8,
    /// 本轮停止是否由壳主动发起（主动停不当作故障上报）。
    stopping_by_request: AtomicBool,
    proxy_on: Mutex<bool>,
    tray_proxy: Mutex<Option<CheckMenuItem<tauri::Wry>>>,
    /// 托盘句柄：核心退出等状态变化时更新 tooltip。
    tray: Mutex<Option<tauri::tray::TrayIcon<tauri::Wry>>>,
    /// 核心 API key 的进程内缓存：首次从 `api-key` 文件读到后记在这里，避免每次管理调用都
    /// 重新走一遍最长 15s 的轮询等待（key 由核心写盘后不再变化，重启核心不影响该缓存）。
    api_key: Mutex<Option<String>>,
    /// 最近一次「核心没能起来」的原因（setup 或 restart_core 失败）。此刻磁盘上的
    /// status.json 只是上一轮的残影，不能当成实时状态推给面板。
    startup_error: Mutex<Option<String>>,
}

/// 壳只注册一次 exit hook（核心侧用 OnceLock 存），因此 app handle 也只需保存一份。
static SHELL_APP: OnceLock<AppHandle> = OnceLock::new();

/// 选监听端口：默认 41980，被占用时由系统分配。
fn pick_port() -> u16 {
    if let Ok(l) = TcpListener::bind(("127.0.0.1", 41980)) {
        let port = l.local_addr().map(|a| a.port()).unwrap_or(41980);
        drop(l);
        return port;
    }
    TcpListener::bind(("127.0.0.1", 0))
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(41980)
}

fn api_key(data_dir: &Path) -> Result<String, String> {
    let file = data_dir.join("api-key");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(text) = fs::read_to_string(&file) {
            let key = text.trim().to_string();
            if !key.is_empty() {
                return Ok(key);
            }
        }
        if Instant::now() >= deadline {
            return Err("读取核心 API key 超时".into());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// 取核心 API key：优先用进程内缓存，未命中再走文件轮询并把结果回填缓存。
fn core_key(state: &AppState) -> Result<String, String> {
    if let Some(key) = state.api_key.lock().unwrap().clone() {
        return Ok(key);
    }
    let key = api_key(&state.data_dir)?;
    *state.api_key.lock().unwrap() = Some(key.clone());
    Ok(key)
}

/// 调核心管理接口（路由表与 `src-tauri/core/src/server.rs` 的 `ACTION_ROUTES` 一一对应）。
fn admin_call(port: u16, key: &str, route: &str, body: Option<&Value>) -> Result<Value, String> {
    admin_call_with(port, key, route, body, Duration::from_secs(60))
}

/// 同上，但可指定超时：退出路径必须用更短的预算（见 graceful_stop_process）。
fn admin_call_with(
    port: u16,
    key: &str,
    route: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<Value, String> {
    let url = format!("http://127.0.0.1:{port}{route}");
    let call = ureq::post(&url)
        .timeout(timeout)
        .set("Authorization", &format!("Bearer {key}"))
        .send_json(body.unwrap_or(&Value::Null));
    let response = match call {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(format!("核心服务请求失败：{e}")),
    };
    // HTTP 状态码才是权威判据：只看响应体里的 error 字段会把「状态码非 2xx 但没带 error」
    // 的响应当成成功（与 `src-tauri/core/src/server.rs` 的路由约定对齐）。
    let status = response.status();
    let text = response.into_string().map_err(|e| e.to_string())?;
    let json = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
    if !(200..300).contains(&status) {
        let message = json.get("error").and_then(|error| error.get("message")).and_then(Value::as_str)
            .unwrap_or("请求失败").to_string();
        return Err(message);
    }
    Ok(json)
}

/// 在壳内启动核心：专用 tokio 运行时 + 显式注入 data_dir/port（同进程改环境变量不安全）。
fn start_core(app: &AppHandle) -> Result<RunningCore, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    fs::create_dir_all(&data_dir).map_err(|e| format!("创建数据目录失败：{e}"))?;
    let port = pick_port();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("wbbridge-core")
        .build()
        .map_err(|e| format!("启动核心异步运行时失败：{e}"))?;
    let state = app.state::<AppState>();
    state.core_stopped.store(false, Ordering::SeqCst);
    state.core_code.store(0, Ordering::SeqCst);
    runtime.spawn(async move {
        let code = orchestration::run(StartOptions {
            data_dir: Some(data_dir.to_string_lossy().into_owned()),
            port: Some(port),
            // 生命周期归壳：核心不注册 SIGTERM/SIGINT，也不看父进程（同进程内无意义）。
            handle_signals: false,
        })
        .await;
        eprintln!("核心编排任务已结束（退出码 {code}）");
    });
    Ok(RunningCore { runtime, port })
}

/// 等待核心编排任务结束（exit hook 已置位）。返回是否在预算内结束。
fn wait_core_stopped(state: &AppState, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if state.core_stopped.load(Ordering::SeqCst) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

/// 停止单个核心实例：HTTP 优雅退出 → 等待编排任务收摊 → 关闭专用运行时。
/// `key` 为 API key（拿不到就直接跳过 HTTP 走超时兜底）。
fn stop_core(core: RunningCore, key: Option<&str>, state: &AppState) {
    let Some(key) = key else {
        // 没有 key 就无法调用 /admin/shutdown；核心的串行写链还在跑，只能给运行时一个
        // 有界的排空窗口，超时会丢弃未完成任务（与 Node 侧强杀等价）。
        core.runtime.shutdown_timeout(Duration::from_secs(5));
        state.core_stopped.store(true, Ordering::SeqCst);
        return;
    };
    state.stopping_by_request.store(true, Ordering::SeqCst);
    // 退出预算必须短：核心正常时 1s 内即退出，卡住时不该让壳等满几十秒（面板点「重启」尤其明显）。
    // 优雅退出 + 等待编排任务收摊，两步都成立才算干净停止；任一不成立都走强制关闭兜底。
    if admin_call_with(core.port, key, "/admin/shutdown", None, Duration::from_secs(5)).is_ok()
        && wait_core_stopped(state, Duration::from_secs(10))
    {
        core.runtime.shutdown_timeout(Duration::from_secs(2));
        return;
    }
    // 优雅路径没走通：给在途任务一个短排空窗口后强制关闭运行时。
    eprintln!("核心未在预算内优雅退出，强制关闭其运行时");
    core.runtime.shutdown_timeout(Duration::from_secs(5));
    state.core_stopped.store(true, Ordering::SeqCst);
}

/// 停止核心：HTTP 优雅退出（核心随后清理 WorkBuddy 配置并写终态）→ 关闭运行时。
fn graceful_stop(state: &AppState) {
    if state.quitting.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(core) = state.core.lock().unwrap().take() else { return };
    let key = core_key(state).ok();
    stop_core(core, key.as_deref(), state);
}

fn show_main(app: &AppHandle) {
    // 退出流程进行中（关闭窗口后到进程结束之间可能有数秒）不要把窗口再唤回来。
    if app.state::<AppState>().quitting.load(Ordering::SeqCst) {
        return;
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// 退出预算：超过则不再等待，交由核心的父进程监护（BUDDY_PARENT_PID）自行收尾。
const STOP_BUDGET: Duration = Duration::from_secs(8);

/// 在后台线程执行 graceful_stop，并在调用线程上有界等待。
/// 停止序列（HTTP 优雅停 → SIGTERM → 强杀）最坏数秒到数十秒，直接在事件循环线程上跑会让
/// 「点退出后应用没反应」，因此下沉到后台线程；即便超时也不再阻塞，核心检测到父进程消失会自行退出。
fn stop_core_bounded(app: &AppHandle) {
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = app.clone();
    thread::spawn(move || {
        graceful_stop(handle.state::<AppState>().inner());
        let _ = tx.send(());
    });
    if rx.recv_timeout(STOP_BUDGET).is_err() {
        eprintln!("核心停止超时（>{STOP_BUDGET:?}），交由核心的父进程监护自行退出");
    }
}

fn quit_app(app: &AppHandle) {
    stop_core_bounded(app);
    app.cleanup_before_exit();
    app.exit(0);
}

/// 非阻塞探测核心是否已经退出；已退出时返回原因描述（退出码 / 信号）。
/// `Child::try_wait` 会缓存退出状态，重复调用返回同一结果，因此可安全地周期探测。
/// 核心是否已结束运行：exit hook 置位即视为已退出，并给出原因描述。
/// 进程内形态没有子进程可探测，终态由核心自己通过 hook 交出（含退出码）。
fn core_exit(state: &AppState) -> Option<String> {
    if !state.core_stopped.load(Ordering::SeqCst) || state.stopping_by_request.load(Ordering::SeqCst) {
        return None;
    }
    Some(format!("退出码 {}", state.core_code.load(Ordering::SeqCst)))
}

/// status.json 的结构版本。与 `src-tauri/core/src/orchestration.rs` 的 `STATUS_SCHEMA_VERSION` 必须同步 +1；
/// 不一致时只提示不阻断（可能只是其中一侧升级了），避免两侧互相锁死。
/// 口径：向后兼容的**加法式**顶层字段（如新增的 `usage`，旧壳忽略未知字段即可）**不**递增本值；
/// 只有删除 / 改名顶层字段或改变既有字段语义（非兼容变更）时才同步 +1。
const STATUS_SCHEMA_VERSION: u64 = 1;

/// 核心当前是否处于「没有服务可看」的故障态，返回（面向面板的原因文案，是否因核心退出）。
///
/// 两个来源：exit hook 报告核心已结束；或壳自己启动失败（setup / restart_core）后没有核心在跑。
/// 用户主动重启/退出（`stopping_by_request`）不算故障，否则重启窗口里会误报「未运行」。
fn service_down(state: &AppState) -> Option<(String, bool)> {
    if let Some(reason) = core_exit(state) {
        return Some((
            format!("核心服务已退出（{reason}），可在面板点“重试”重启"),
            true,
        ));
    }
    let running = state.core.lock().unwrap().is_some();
    if !running {
        if let Some(message) = state.startup_error.lock().unwrap().clone() {
            return Some((message, false));
        }
    }
    None
}

/// status.json 这一轮是否需要真读内容。快路径**只在拿得到 mtime 时**才允许跳过：
/// `stamp` 为 `None`（文件系统不暴露 mtime，如某些网络挂载 / FUSE）时必须回落到读内容，
/// 否则 stamp 恒定、长度不变的改写会被永久跳过，面板状态就此冻结。
fn status_read_needed(
    stamp: Option<(std::time::SystemTime, u64)>,
    last_stamp: Option<(std::time::SystemTime, u64)>,
) -> bool {
    stamp.is_none() || stamp != last_stamp
}

/// 轮询核心写的 status.json：内容变化即推送事件、同步托盘勾选态，并监督核心任务是否已结束。
fn watch_status(app: AppHandle) {
    thread::spawn(move || {
        let state = app.state::<AppState>();
        let file = state.data_dir.join("status.json");
        let mut last = String::new();
        // 上一次成功读取时的 (mtime, 长度)：稳态下核心不写盘、内容一成不变，
        // 原先每 500ms 仍要 read_to_string 一次（实测文件 8.6 KB）再整份比对。
        // 先用 mtime + 长度做便宜的前置过滤，两者都不变才跳过；拿不到 mtime 时为 None，
        // 快路径自动失效（见 status_read_needed）。
        let mut last_stamp: Option<(std::time::SystemTime, u64)> = None;
        let mut warned = false;
        // 已向面板播报的故障原因：原因变化或攒满一个重播窗口才再发，避免每 500ms 刷屏。
        let mut announced: Option<String> = None;
        let mut ticks = 0usize;
        // 退出后不再轮询、不再向已关闭的窗口 emit。
        while !state.quitting.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(500));
            ticks = ticks.wrapping_add(1);
            // 故障态下 status.json 停在上一轮的 ready（甚至更早），把它当实时状态推出去就等于
            // 让面板显示「就绪」却没有核心在跑、连重试入口都不见；改为持续播报失败原因，
            // 直到晚挂载的面板监听器收到（setup 里那一次 emit 必然早于监听注册）。
            if let Some((message, by_exit)) = service_down(&state) {
                if announced.as_deref() != Some(message.as_str()) || ticks.is_multiple_of(8) {
                    announced = Some(message.clone());
                    if let Some(tray) = state.tray.lock().unwrap().as_ref() {
                        let _ = tray.set_tooltip(Some(if by_exit {
                            "WB Bridge — 核心服务已退出，可在面板重启"
                        } else {
                            "WB Bridge — 核心服务未运行，可在面板重启"
                        }));
                    }
                    let _ = app.emit("core-failed", serde_json::json!({ "message": message }));
                }
                continue;
            }
            // 恢复运行：清掉故障播报标记并还原托盘 tooltip，同时作废「上一份内容」与「上一次
            // stamp」两重缓存 —— 重启后写出的 status.json 可能与故障前逐字节相同，不重置就
            // 永远不会再推送；只清 `last` 而留下 stamp，这道保障会被快路径吞掉。
            if announced.take().is_some() {
                last.clear();
                last_stamp = None;
                if let Some(tray) = state.tray.lock().unwrap().as_ref() {
                    let _ = tray.set_tooltip(Some("WB Bridge"));
                }
            }
            // 先用 (mtime, 长度) 做便宜的前置过滤：核心只在变化时写盘，稳态下这两者都不动，
            // 就不必每 500ms 再 read_to_string 一次整份内容（实测 status.json 8.6 KB）
            // 后再逐字节比对。stamp 只在成功读到内容之后记录，避免读失败的那一轮被跳过。
            let Ok(meta) = fs::metadata(&file) else { continue };
            let stamp = meta.modified().ok().map(|mtime| (mtime, meta.len()));
            if !status_read_needed(stamp, last_stamp) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&file) else { continue };
            last_stamp = stamp;
            if text == last {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<Value>(&text) else { continue };
            last = text;
            if !warned {
                match parsed.get("schemaVersion").and_then(Value::as_u64) {
                    Some(STATUS_SCHEMA_VERSION) => {}
                    Some(found) => {
                        warned = true;
                        eprintln!("status.json schemaVersion={found} 与壳预期的 {STATUS_SCHEMA_VERSION} 不一致，面板按可用字段渲染");
                    }
                    None => {
                        warned = true;
                        eprintln!("status.json 缺少 schemaVersion（核心可能为旧版），建议重新构建核心");
                    }
                }
            }
            let proxy = parsed.get("useSystemProxy").and_then(Value::as_bool).unwrap_or(false);
            *state.proxy_on.lock().unwrap() = proxy;
            if let Some(item) = state.tray_proxy.lock().unwrap().as_ref() {
                let _ = item.set_checked(proxy);
            }
            // 事件拆两路：core-status 去掉 activity / modelResults 两个大数组，只推轻量状态；
            // core-activity 推活动与模型明细。避免每 500ms 跨 IPC 搬运整份 status.json。
            let mut light = parsed.clone();
            let activity = light.get_mut("activity").map(std::mem::take).unwrap_or(Value::Null);
            let results = light.get_mut("modelResults").map(std::mem::take).unwrap_or(Value::Null);
            let _ = app.emit("core-status", light);
            let _ = app.emit("core-activity", serde_json::json!({ "activity": activity, "modelResults": results }));
        }
    });
}

fn handle_menu(app: &AppHandle, id: &str) {
    match id {
        "open" => show_main(app),
        "quit" => quit_app(app),
        "proxy" => {
            let state = app.state::<AppState>();
            let next = !*state.proxy_on.lock().unwrap();
            let core = state.core.lock().unwrap().as_ref().map(|c| (c.port, state.data_dir.clone()));
            if let Some((port, data_dir)) = core {
                thread::spawn(move || {
                    let Ok(key) = api_key(&data_dir) else {
                        eprintln!("[tray] 无法读取 api-key，系统代理切换未执行");
                        return;
                    };
                    // 失败必须留下痕迹：托盘勾选会在下一次状态轮询被核心权威值纠正，
                    // 但若不记录原因，用户只会看到勾选自己弹回去。
                    if let Err(error) =
                        admin_call(port, &key, "/admin/system-proxy", Some(&serde_json::json!({ "enabled": next })))
                    {
                        eprintln!("[tray] 系统代理切换失败：{error}");
                    }
                });
            }
        }
        "pick" => {
            let app = app.clone();
            app.dialog().file()
                .add_filter("WorkBuddy models.json", &["json"])
                .pick_file(move |path| {
                    let Some(path) = path else { return };
                    let app = app.clone();
                    thread::spawn(move || {
                        let state = app.state::<AppState>();
                        let core = state.core.lock().unwrap().as_ref().map(|c| c.port);
                        if let Some(port) = core {
                            match (api_key(&state.data_dir), admin_route("import")) {
                                (Ok(key), Ok(route)) => {
                                    // 导入失败只在日志留痕：面板的 FeedbackBar 由 run() 的返回值驱动，
                                    // 托盘这条链路没有可复用的错误展示位。
                                    if let Err(error) = admin_call(
                                        port,
                                        &key,
                                        route,
                                        Some(&serde_json::json!({ "modelsFile": path.to_string() })),
                                    ) {
                                        eprintln!("[tray] 导入 WorkBuddy 配置失败：{error}");
                                    }
                                }
                                (Err(key), _) => eprintln!("[tray] 无法读取 api-key，导入未执行：{key}"),
                                (_, Err(route)) => eprintln!("[tray] 导入动作路由缺失：{route}"),
                            }
                        }
                        show_main(&app);
                    });
                });
        }
        _ => {}
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "打开控制面板", true, None::<&str>)?;
    let proxy = CheckMenuItem::with_id(app, "proxy", "使用系统代理", true, false, None::<&str>)?;
    let pick = MenuItem::with_id(app, "pick", "选择 WorkBuddy 配置…", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "退出 WB Bridge", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &sep1, &proxy, &pick, &sep2, &quit])?;
    let icon = if cfg!(target_os = "macos") {
        tauri::include_image!("icons/tray.png")
    } else {
        tauri::include_image!("icons/32x32.png")
    };
    let state = app.state::<AppState>();
    *state.tray_proxy.lock().unwrap() = Some(proxy.clone());
    TrayIconBuilder::with_id("main")
        .icon(icon)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("WB Bridge")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| handle_menu(app, event.id().as_ref()))
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: tauri::tray::MouseButtonState::Up, .. } = event {
                show_main(tray.app_handle());
            }
        })
        .build(app)
        .map(|tray| *state.tray.lock().unwrap() = Some(tray))?;
    Ok(())
}

/// 面板动作 → 核心路由的映射表。与 src-tauri/core/src/server.rs 的 ACTION_ROUTES 逐项一致
/// （由本文件的 `shell_action_routes_match_the_core_contract` 断言）；docs/contract.md 的契约表
/// 是同一份内容的文字版，改动时仍需人工同步。
const ADMIN_ROUTES: [(&str, &str); 8] = [
    ("refresh", "/admin/refresh"),
    ("probe", "/admin/probe"),
    ("import", "/admin/import"),
    ("system-proxy", "/admin/system-proxy"),
    ("shutdown", "/admin/shutdown"),
    ("provider-status", "/admin/provider-status"),
    ("set-provider-key", "/admin/set-provider-key"),
    ("clear-provider-key", "/admin/clear-provider-key"),
];

fn admin_route(action: &str) -> Result<&'static str, String> {
    ADMIN_ROUTES
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, route)| *route)
        .ok_or_else(|| format!("未知操作：{action}"))
}

/// 调核心管理接口的公共封装：锁的作用域只到「取出端口」为止，真正阻塞的调用
/// （api_key 最多轮询 15s、admin_call 超时 60s）下沉到 spawn_blocking。
/// 留在同步命令里会占住主线程，期间 restart_core、core_running、
/// graceful_stop（含退出流程）全部串行卡死，面板与托盘一起无响应。
async fn admin_command(
    app: AppHandle,
    state: State<'_, AppState>,
    route: &'static str,
    payload: Option<Value>,
) -> Result<Value, String> {
    let port = {
        let guard = state.core.lock().unwrap();
        guard.as_ref().map(|core| core.port)
    };
    let Some(port) = port else { return Err("核心服务未运行".into()) };
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let key = core_key(&state)?;
        admin_call(port, &key, route, payload.as_ref())
    })
    .await
    .map_err(|e| format!("动作线程已崩溃：{e}"))?
}

/// 面板动作一律以**类型化命令**暴露（替换旧的 `core_action` 通用代理）：
/// - 动作名不再是任意字符串：八个动作各对应一条命令（即 [`ADMIN_ROUTES`] / 核心
///   `ACTION_ROUTES` 的八条路由），未知命令在 IPC 层根本不存在，无法再拼任意动作名；
/// - 每个命令的参数都是强类型字段，serde 反序列化即完成校验：字段名或类型错误在 IPC 层
///   直接拒绝；payload 由壳侧按该动作的契约组装后再转发，调用方不再能送任意请求体；
/// - 与既往前端契约保持形状兼容：`refresh / shutdown / provider-status` 无参数，
///   `probe` 收可选 `model / providers`，`import` 收可选 `modelsFile`，
///   `system-proxy` 必填 `enabled`，`set-provider-key` 必填 `provider + apiKey`，
///   `clear-provider-key` 必填 `provider`（后两者的 provider 合法性仍由核心裁决，
///   壳层不重复业务校验，保持单一真相）。

#[tauri::command]
async fn core_refresh(app: AppHandle, state: State<'_, AppState>) -> Result<Value, String> {
    admin_command(app, state, "/admin/refresh", None).await
}

#[tauri::command]
async fn core_probe(
    app: AppHandle,
    state: State<'_, AppState>,
    model: Option<String>,
    providers: Option<Vec<String>>,
) -> Result<Value, String> {
    let mut payload = serde_json::Map::new();
    if let Some(model) = model {
        payload.insert("model".into(), Value::String(model));
    }
    if let Some(providers) = providers {
        payload.insert("providers".into(), serde_json::to_value(providers).unwrap_or(Value::Null));
    }
    admin_command(app, state, "/admin/probe", Some(Value::Object(payload))).await
}

#[tauri::command]
async fn core_import(
    app: AppHandle,
    state: State<'_, AppState>,
    models_file: Option<String>,
) -> Result<Value, String> {
    let payload = models_file.map(|file| serde_json::json!({ "modelsFile": file }));
    admin_command(app, state, "/admin/import", payload).await
}

#[tauri::command]
async fn core_system_proxy(
    app: AppHandle,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<Value, String> {
    admin_command(app, state, "/admin/system-proxy", Some(serde_json::json!({ "enabled": enabled }))).await
}

#[tauri::command]
async fn core_shutdown(app: AppHandle, state: State<'_, AppState>) -> Result<Value, String> {
    admin_command(app, state, "/admin/shutdown", None).await
}

#[tauri::command]
async fn core_provider_status(app: AppHandle, state: State<'_, AppState>) -> Result<Value, String> {
    admin_command(app, state, "/admin/provider-status", None).await
}

#[tauri::command]
async fn core_set_provider_key(
    app: AppHandle,
    state: State<'_, AppState>,
    provider: String,
    api_key: String,
) -> Result<Value, String> {
    admin_command(
        app,
        state,
        "/admin/set-provider-key",
        Some(serde_json::json!({ "provider": provider, "apiKey": api_key })),
    )
    .await
}

#[tauri::command]
async fn core_clear_provider_key(
    app: AppHandle,
    state: State<'_, AppState>,
    provider: String,
) -> Result<Value, String> {
    admin_command(
        app,
        state,
        "/admin/clear-provider-key",
        Some(serde_json::json!({ "provider": provider })),
    )
    .await
}

/// 面板可以请求打开的官方站点主机名（与 `src/views/ProvidersView.vue` 的 `PLATFORM_GUIDES`
/// 一一对应；新增平台必须同步改这两处）。
///
/// 为什么不能只校验「https + 合法字符」：那样这条命令就是**任意 https 的外带通道**——面板
/// 一旦出现注入面，攻击者可把本机状态拼进 query 让系统浏览器发往自己的域名，还能无提示拉起
/// 浏览器。只认精确主机名（并禁掉 userinfo 与端口）后，`https://www.modelscope.cn@evil.tld/`
/// 这类视觉伪装与后缀拼接（`modelscope.cn.evil.tld`）都不可能通过。
const EXTERNAL_HOSTS: &[&str] = &[
    "www.modelscope.cn",
    "cloud.siliconflow.cn",
    "console.cloud.tencent.com",
    "open.bigmodel.cn",
];

/// `open_external` 的校验部分，返回 `None` 即放行。
///
/// 抽成纯函数是因为这层边界必须能被单测钉住（真调用会拉起系统浏览器，不可测也不该测）。
fn external_url_rejection(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    if !lower.starts_with("https://") {
        return Some(format!("不允许的链接协议（仅 https）：{url}"));
    }
    if url.len() > 2048 {
        return Some("链接长度非法".to_string());
    }
    const URL_SAFE: &[u8] = b":/?#[]@!$&'()*+,;=-._~%";
    if !url
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || URL_SAFE.contains(&c))
    {
        return Some("链接包含非法字符".to_string());
    }
    // authority = `https://` 之后、第一个 `/`·`?`·`#` 之前的那一段（前面已确认是 ASCII）。
    let authority = lower["https://".len()..]
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("");
    if authority.contains('@') || authority.contains(':') {
        return Some("链接不允许携带用户信息或端口".to_string());
    }
    if EXTERNAL_HOSTS.contains(&authority) {
        return None;
    }
    Some(format!("链接主机不在已登记的官方站点名单内：{authority}"))
}

/// 用系统默认浏览器打开外部链接（申请 Key 的官方页等）。
///
/// Tauri WebView 里 `target="_blank"` 默认不调起系统浏览器（点击静默失败），外链必须经壳转发。
/// 安全边界：https + RFC 3986 字符白名单 + [`EXTERNAL_HOSTS`] 精确主机名（见其注释）。
/// URL 只以命令行参数传给系统打开器（macOS `open` / Windows `explorer` 直接传参不经 shell
/// 解析 / Linux `xdg-open`），名单是最后一道防线，不依赖调用方的引号处理。
/// 应用自有命令，不经 capability 授权。
#[tauri::command]
fn open_external(url: String) -> Result<(), String> {
    if let Some(error) = external_url_rejection(&url) {
        return Err(error);
    }

    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(&url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        // explorer 直接传参：不经 cmd /C shell 解析，杜绝元字符注入。
        let mut c = std::process::Command::new("explorer");
        c.arg(&url);
        c
    };
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(&url);
        c
    };
    cmd.spawn().map_err(|e| format!("打开链接失败: {e}"))?;
    Ok(())
}

#[tauri::command]
async fn restart_core(app: AppHandle) -> Result<(), String> {
    // 同上：stop_core 最坏等十余秒，必须离开主线程；AppHandle 是 'static，可在闭包内重新取状态。
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        restart_core_with(&app, &state)
    })
    .await
    .map_err(|e| format!("重启线程已崩溃：{e}"))?
}

/// 重启主体的既有逻辑（不含线程调度）：停旧实例 → 启新实例 → 记录/清除失败原因。
fn restart_core_with(app: &AppHandle, state: &State<AppState>) -> Result<(), String> {
    // 锁的作用域只到「取出句柄」为止：stop_core 会走 HTTP 优雅退出并等待编排任务收摊，最坏
    // 十余秒，持锁期间 core_running、core_* 动作命令与退出流程都会被一并阻塞。
    let existing = state.core.lock().unwrap().take();
    if let Some(core) = existing {
        let key = core_key(state).ok();
        stop_core(core, key.as_deref(), state);
    }
    state.stopping_by_request.store(false, Ordering::SeqCst);
    let core = match start_core(app) {
        Ok(core) => core,
        Err(message) => {
            *state.startup_error.lock().unwrap() = Some(message.clone());
            let _ = app.emit("core-failed", serde_json::json!({ "message": message }));
            return Err(message);
        }
    };
    *state.startup_error.lock().unwrap() = None;
    *state.core.lock().unwrap() = Some(core);
    Ok(())
}

#[tauri::command]
fn core_running(state: State<AppState>) -> bool {
    let guard = state.core.lock().unwrap();
    guard.is_some() && !state.core_stopped.load(Ordering::SeqCst)
}

/// 数据目录路径（壳实际使用的 `app_data_dir`）。原命令名 `open_models_config` 名不副实：
/// 它既不打开任何文件、也不指向 models.json，只返回 status.json 的路径。改为直接返回目录，
/// 由调用方按需拼接文件名，避免两处各写一套 app_data_dir 逻辑。
#[tauri::command]
fn data_dir_path(app: AppHandle) -> Result<String, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(dir.to_string_lossy().into_owned())
}

/// 日志文件名：与核心 `orchestration.rs` 的 `join_host(&[&data_dir, "opencode.log"])` 一致，
/// 轮转文件是 `<同一路径>.previous`。两处必须同步改名，否则面板会静默读到空日志。
const LOG_FILE_NAME: &str = "opencode.log";
/// 尾部字节上限：日志按 5MB 轮转，面板只看最近一段，读全量既无意义也会把大字符串塞进 IPC。
const LOG_TAIL_BYTES: u64 = 256 * 1024;
/// 尾部行数上限（比字节上限更早生效时也要报告 truncated）。
const LOG_TAIL_LINES: usize = 1200;

/// 运行日志（只读）：返回数据目录下核心日志的尾部。
///
/// 路径由壳自己按 `app_data_dir` + 固定文件名拼出，**不接受调用方传路径** —— 面板只读展示，
/// 不得借这条命令读取数据目录之外的任意文件（也不写文件）。
/// 返回 `{ text, truncated, bytes }`：`bytes` 是日志文件的当前总字节数（不是返回文本长度），
/// `truncated` 表示尾部内容因字节/行数上限被截断。
#[tauri::command]
fn read_log(app: AppHandle) -> Result<Value, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    read_log_tail(&dir.join(LOG_FILE_NAME), LOG_TAIL_BYTES, LOG_TAIL_LINES)
}

/// 清空运行日志：把数据目录下的日志文件截断为空。
///
/// 边界与 `read_log` 一致：路径由壳按 `app_data_dir` + 固定文件名拼出，**不接受调用方传路径**；
/// 只截断当前日志文件，不碰轮转存档 `opencode.log.previous`（面板不读它，留着便于排查旧问题）。
/// 核心以 append 模式持有该文件（`orchestration.rs`），外部截断后新写入仍从新的末尾追加，
/// 无需通知核心。文件不存在（核心还没写过日志）视为本就为空，不报错。
#[tauri::command]
fn clear_log(app: AppHandle) -> Result<Value, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    clear_log_file(&dir.join(LOG_FILE_NAME))
}

/// 读取日志尾部（纯函数，便于单测）。
///
/// 实现要点：
/// - 只 seek 到 `total - max_bytes` 起读，不把 5MB 日志整体读进内存；
/// - 从中间字节开始解码时首行必然是断的，丢掉这半行（若整个尾部就是一行超长文本则无从丢弃，
///   此时 `truncated` 已为 true，面板会提示内容被截断）；
/// - 文件不存在（核心还没写过日志）按空日志处理，不报错。
fn read_log_tail(path: &Path, max_bytes: u64, max_lines: usize) -> Result<Value, String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(serde_json::json!({ "text": "", "truncated": false, "bytes": 0 }))
        }
        Err(error) => return Err(format!("读取运行日志失败：{error}")),
    };
    let total = file
        .metadata()
        .map_err(|error| format!("读取运行日志失败：{error}"))?
        .len();
    let mut truncated = total > max_bytes;
    if truncated {
        file.seek(SeekFrom::Start(total - max_bytes))
            .map_err(|error| format!("读取运行日志失败：{error}"))?;
    }
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)
        .map_err(|error| format!("读取运行日志失败：{error}"))?;
    let mut text = String::from_utf8_lossy(&buffer).into_owned();
    if truncated {
        if let Some(index) = text.find('\n') {
            text.drain(..=index);
        }
    }
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.len() > max_lines {
        lines.drain(..lines.len() - max_lines);
        truncated = true;
    }
    Ok(serde_json::json!({
        "text": lines.join("\n"),
        "truncated": truncated,
        "bytes": total,
    }))
}

/// 截断日志文件（纯函数，便于单测）：`write + truncate` 在打开文件这一步就把它清成空。
fn clear_log_file(path: &Path) -> Result<Value, String> {
    match fs::OpenOptions::new().write(true).truncate(true).open(path) {
        Ok(_file) => Ok(serde_json::json!({ "cleared": true })),
        // 与 read_log 的缺失语义一致：核心还没写过日志，本就没有要清的内容。
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(serde_json::json!({ "cleared": false }))
        }
        Err(error) => Err(format!("清空运行日志失败：{error}")),
    }
}

// ── AI 助手：OpenAI 兼容端点的流式转发 ───────────────────────────────
// 面板 CSP 是 default-src 'self'（connect-src 只放行 ipc:），前端 fetch 外部端点会被直接
// 拦掉，因此 AI 对话统一走壳命令：壳用 ureq 发请求、按行解析 SSE，增量经事件推给面板。
// 凭据纪律：apiKey 只经命令参数进入进程内存，不落盘、不进日志、不出现在任何事件 payload 里；
// Base URL 白名单（https，或本机回环 http）在壳侧校验，前端传什么都绕不过它。

/// AI 请求参数（与前端 `src/core/ai.js` 的调用约定一致）。
///
/// 两种来源共用这一条命令：
/// - 外部端点：`baseUrl` + `apiKey`（https，或本机回环 http 的本地模型）；
/// - 自托管（`selfHosted = true`）：大脑就是本壳正在运行的核心，端点与 Bearer 令牌
///   都由壳自己解析（`core.port` + `core_key`），**面板既拿不到也不需要 api-key**，
///   因此这条路径上 `baseUrl` / `apiKey` 一律忽略，前端无从指定任意回环地址。
#[derive(serde::Deserialize)]
struct AiChatRequest {
    #[serde(rename = "chatId")]
    chat_id: String,
    #[serde(rename = "baseUrl", default)]
    base_url: String,
    #[serde(rename = "apiKey", default)]
    api_key: String,
    #[serde(rename = "selfHosted", default)]
    self_hosted: bool,
    model: String,
    messages: Vec<Value>,
    /// OpenAI function tools 声明。缺省空数组 = 本轮不启用工具（纯文本对话）。
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(rename = "toolChoice", default)]
    tool_choice: Option<Value>,
}

/// 进行中的 AI 对话取消标志（chatId → flag）：`ai_abort` 置位，流式线程读到即停。
static AI_CANCELS: OnceLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = OnceLock::new();

fn ai_cancels() -> &'static Mutex<HashMap<String, Arc<AtomicBool>>> {
    AI_CANCELS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cleanup_cancel(chat_id: &str) {
    ai_cancels().lock().unwrap().remove(chat_id);
}

/// 入参体积闸门：对话历史与工具声明都由面板构造，长度失控时既拖慢上游也会把整段上下文
/// 留在壳内存里。数值取「足够跑完一轮正常管理对话、又远小于 8MB IPC 上限」的一档。
const AI_MAX_MESSAGES: usize = 40;
const AI_MAX_MESSAGE_CHARS: usize = 30_000;
const AI_MAX_TOOLS: usize = 40;

/// 校验请求形态（纯函数，便于单测）。返回归一化后的 Base URL，自托管模式返回空串。
fn validate_ai_request(request: &AiChatRequest) -> Result<String, String> {
    if request.chat_id.trim().is_empty() {
        return Err("AI 请求缺少 chatId".into());
    }
    if request.model.trim().is_empty() {
        return Err("AI 请求缺少 model".into());
    }
    if request.messages.is_empty() {
        return Err("AI 请求缺少 messages".into());
    }
    if request.messages.len() > AI_MAX_MESSAGES {
        return Err(format!("对话最多保留 {AI_MAX_MESSAGES} 条消息，请清空历史后重试"));
    }
    for message in &request.messages {
        let chars = message.to_string().chars().count();
        if chars > AI_MAX_MESSAGE_CHARS {
            return Err(format!("单条消息过长（{chars} 字符），请缩短后重试"));
        }
    }
    if request.tools.len() > AI_MAX_TOOLS {
        return Err(format!("工具声明最多 {AI_MAX_TOOLS} 个"));
    }
    if request.self_hosted {
        return Ok(String::new());
    }
    normalize_ai_base_url(&request.base_url)
}

/// 组装上游请求体（纯函数，便于单测）。只声明工具时才带 `tools`/`tool_choice`，
/// 免得把 `tools: []` 发给不认识该字段的老端点。
fn build_ai_body(request: &AiChatRequest) -> Value {
    let mut body = serde_json::json!({
        "model": request.model,
        "messages": request.messages,
        "stream": true,
    });
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.clone());
        body["tool_choice"] = request
            .tool_choice
            .clone()
            .unwrap_or_else(|| serde_json::json!("auto"));
    }
    body
}

/// 自托管端点：只指向本壳内核心的回环地址，端口来自壳自己持有的 `CoreHandle`。
fn self_hosted_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

/// 自托管模式的端点与令牌：端口取正在运行的核心，Bearer 用数据目录里的 `api-key`。
/// 两者都不经面板流转 —— WebView 从来不知道本机密钥，「api-key 不出面板」这条红线因此
/// 不被触碰，用户也不需要为本机模型再填一次 Key。
fn resolve_self_hosted(app: &AppHandle) -> Result<(String, String), String> {
    let state = app.state::<AppState>();
    // 锁的作用域只到取出端口为止：core_key 最坏会轮询 15s 等核心写入 api-key 文件，
    // 绝不能握着 core 锁去做（restart_core / core_running / 退出流程都会被它卡死）。
    let port = {
        let guard = state.core.lock().unwrap();
        guard.as_ref().map(|core| core.port)
    };
    let Some(port) = port else {
        return Err("核心服务未运行，本机模型不可用；请改用外部端点".into());
    };
    let key = core_key(&state)?;
    Ok((
        format!("{}/chat/completions", self_hosted_base_url(port)),
        key,
    ))
}

/// 校验 AI 端点 Base URL 并归一化（去掉尾部斜杠）。只允许 https，或本机回环 http
/// （本地 Ollama / oMLX 一类）；其余一律拒绝——壳是面板出网的唯一出口，入口必须收窄。
/// `http://127.0.0.1.evil.com` 这类前缀伪装按主机名精确比对挡掉。
fn normalize_ai_base_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return Err(format!(
            "AI 服务地址需以 http:// 或 https:// 开头（收到：{trimmed}）"
        ));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // 去掉 userinfo（http://user@host/ 形式），再取主机名（IPv6 按 [..] 字面量解析）。
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host_name = if let Some(inner) = host_port.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    }
    .to_ascii_lowercase();
    if host_name.is_empty() {
        return Err("AI 服务地址缺少主机名".into());
    }
    let local = matches!(host_name.as_str(), "127.0.0.1" | "localhost" | "::1");
    let allowed = matches!(scheme.to_ascii_lowercase().as_str(), "https")
        || (local && scheme.eq_ignore_ascii_case("http"));
    if !allowed {
        return Err(format!(
            "仅支持 https:// 端点，或 http://127.0.0.1 / http://localhost 本地模型（收到：{trimmed}）"
        ));
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

/// OpenAI 流式 chunk 的累积器：content 增量直接拼接；tool_calls 按 index 累积
/// （id / name 通常只在首片出现，arguments 是字符串分片、必须拼接）。
#[derive(Default)]
struct AiStreamAcc {
    content: String,
    tool_calls: Vec<AiToolCallAcc>,
    finish_reason: String,
}

#[derive(Default)]
struct AiToolCallAcc {
    id: String,
    name: String,
    arguments: String,
}

impl AiStreamAcc {
    /// 吸收一个 SSE data 段的 JSON。返回本次的文本增量（调用方据此节流推送）。
    fn absorb(&mut self, chunk: &Value) -> String {
        let mut delta = String::new();
        let Some(choice) = chunk
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            return delta;
        };
        if let Some(text) = choice
            .get("delta")
            .and_then(|d| d.get("content"))
            .and_then(Value::as_str)
        {
            self.content.push_str(text);
            delta.push_str(text);
        }
        if let Some(calls) = choice
            .get("delta")
            .and_then(|d| d.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                while self.tool_calls.len() <= index {
                    self.tool_calls.push(AiToolCallAcc::default());
                }
                let slot = &mut self.tool_calls[index];
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    if !id.is_empty() && slot.id.is_empty() {
                        slot.id = id.to_string();
                    }
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        if !name.is_empty() && slot.name.is_empty() {
                            slot.name = name.to_string();
                        }
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        slot.arguments.push_str(arguments);
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            if !reason.is_empty() {
                self.finish_reason = reason.to_string();
            }
        }
        delta
    }

    /// 收尾 payload：没有 name 的空槽（部分提供商只发 index 不带内容）不进最终数组。
    fn finish(&self) -> Value {
        serde_json::json!({
            "content": self.content,
            "toolCalls": self
                .tool_calls
                .iter()
                .filter(|call| !call.name.is_empty())
                .map(|call| serde_json::json!({
                    "id": call.id,
                    "name": call.name,
                    "arguments": call.arguments,
                }))
                .collect::<Vec<_>>(),
            "finishReason": self.finish_reason,
        })
    }
}

/// 错误响应体截断：压掉换行后取前 200 字符（端点错误页可能是一整页 HTML）。
fn snippet_of(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = collapsed.chars().count();
    let mut out: String = collapsed.chars().take(200).collect();
    if count > 200 {
        out.push('…');
    }
    out
}

/// 流中错误帧的识别：SSE 响应头一旦发出（核心 `/v1/chat/completions` 的流式分支先写
/// 校验注释帧、立刻回 200），后续失败只能以 `data: {"error":{...}}` 补发且没有 `[DONE]`
/// （`server.rs::StreamJob` 失败路径的唯一出口）。这类帧没有 `choices`，累加器会整帧丢弃；
/// 不在这里识别，面板拿到的就是「空内容 + 无工具调用」的成功——表现为转圈结束后气泡一片
/// 空白、不报任何原因。返回可读的错误文案；非错误帧返回 None。
fn sse_error_of(chunk: &Value) -> Option<String> {
    let error = chunk.get("error").filter(|value| !value.is_null())?;
    let text = match error.as_str() {
        // 少数端点把 error 写成字符串。其余形态优先 message、次选 code，都缺再退化成整段 JSON。
        Some(text) => text.to_string(),
        None => error
            .get("message")
            .or_else(|| error.get("code"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string()),
    };
    if text.trim().is_empty() {
        return None;
    }
    Some(format!("AI 端点在流中返回错误：{}", snippet_of(&text)))
}

/// AI 对话（流式）：壳代面板向 OpenAI 兼容端点发请求，SSE 增量经 `ai-chunk` 事件推送，
/// 结束 / 失败 / 取消分别发 `ai-done` / `ai-error` / `ai-aborted`（payload 都带 chatId）。
/// 同步返回、请求在独立线程里跑，命令本身不等待。
#[tauri::command]
fn ai_chat(app: AppHandle, payload: Value) -> Result<(), String> {
    let request: AiChatRequest =
        serde_json::from_value(payload).map_err(|error| format!("AI 请求参数无效：{error}"))?;
    // 形态与端点白名单在起线程之前判完，错误能直接回到调用方（面板据此显示失败原因）。
    let base_url = validate_ai_request(&request)?;
    let url = if request.self_hosted {
        // 自托管的端点与令牌在流式线程里解析：core_key 可能轮询 api-key 文件，
        // 留在命令线程上会冻住 WebView（与 core_action 同一处坑）。
        String::new()
    } else {
        format!("{base_url}/chat/completions")
    };
    let cancel = {
        let mut guards = ai_cancels().lock().unwrap();
        guards
            .entry(request.chat_id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    };
    let chat_id = request.chat_id.clone();
    thread::spawn(move || {
        stream_ai_chat(app, chat_id, url, request, cancel);
    });
    Ok(())
}

/// 中止进行中的 AI 对话：只置取消标志，真正的断连由流式线程在下一次读取检查时完成。
#[tauri::command]
fn ai_abort(chat_id: String) {
    if let Some(flag) = ai_cancels().lock().unwrap().get(&chat_id) {
        flag.store(true, Ordering::SeqCst);
    }
}

/// 流式读取线程主体。任何出口都要发终态事件，否则前端的 Promise 永远悬挂
/// （发送按钮会卡在「停止」态）。
fn stream_ai_chat(
    app: AppHandle,
    chat_id: String,
    url: String,
    request: AiChatRequest,
    cancel: Arc<AtomicBool>,
) {
    // 自托管：端点与 Bearer 令牌都取自壳自己持有的核心句柄。这段放在线程里而不是命令里，
    // 因为 core_key 在最坏情况会轮询等待核心写入 api-key 文件（阻塞调用不得留在主线程）。
    let (url, api_key) = if request.self_hosted {
        match resolve_self_hosted(&app) {
            Ok(resolved) => resolved,
            Err(error) => {
                let _ = app.emit(
                    "ai-error",
                    serde_json::json!({ "chatId": chat_id, "error": error }),
                );
                cleanup_cancel(&chat_id);
                return;
            }
        }
    } else {
        (url, request.api_key.clone())
    };
    let body = build_ai_body(&request);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        // 流式响应的「空闲超时」：距上一块数据超过 90s 视为断流（生成中的长停顿由
        // 服务端 keep-alive 续命；整体时长不设上限）。
        .timeout_read(Duration::from_secs(90))
        .build();
    let mut sent = agent
        .post(&url)
        .set("Content-Type", "application/json")
        .set("Accept", "text/event-stream");
    if !api_key.trim().is_empty() {
        sent = sent.set("Authorization", &format!("Bearer {}", api_key.trim()));
    }
    let response = match sent.send_json(&body) {
        Ok(response) => response,
        Err(ureq::Error::Status(status, response)) => {
            let detail = response.into_string().unwrap_or_default();
            let hint = match status {
                401 | 403 => "端点拒绝了 API Key（检查是否填写正确）".to_string(),
                404 => "端点不存在（Base URL 是否以 /v1 结尾？）".to_string(),
                429 => "端点限流（429），请稍后再试".to_string(),
                _ => format!("HTTP {status}"),
            };
            let snippet = snippet_of(&detail);
            let error = if snippet.is_empty() {
                hint
            } else {
                format!("{hint}：{snippet}")
            };
            let _ = app.emit("ai-error", serde_json::json!({ "chatId": chat_id, "error": error }));
            cleanup_cancel(&chat_id);
            return;
        }
        Err(error) => {
            let _ = app.emit(
                "ai-error",
                serde_json::json!({ "chatId": chat_id, "error": format!("请求 AI 端点失败：{error}") }),
            );
            cleanup_cancel(&chat_id);
            return;
        }
    };
    let reader = std::io::BufReader::new(response.into_reader());
    let mut acc = AiStreamAcc::default();
    let mut aborted = false;
    let mut failed: Option<String> = None;
    for line in std::io::BufRead::lines(reader) {
        if cancel.load(Ordering::SeqCst) {
            aborted = true;
            break;
        }
        let line = match line {
            Ok(line) => line,
            // 读超时 / 连接中断：已收到的部分随错误一起交给前端（气泡内容不丢）。
            Err(error) => {
                failed = Some(format!("读取 AI 响应中断：{error}"));
                break;
            }
        };
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }
        // 单行解析失败不影响整条流（部分提供商会夹带注释行 / keep-alive）。
        if let Ok(chunk) = serde_json::from_str::<Value>(data) {
            // 流中错误帧必须在累加之前判：它没有 choices，先进累加器就等于被吞掉，
            // 面板会拿到「成功但什么都没有」——这正是 WB · auto 落到慢/坏免费模型时
            // 「没有任何信息返回」的根源。
            if let Some(message) = sse_error_of(&chunk) {
                failed = Some(message);
                break;
            }
            let delta = acc.absorb(&chunk);
            if !delta.is_empty() {
                let _ = app.emit("ai-chunk", serde_json::json!({ "chatId": chat_id, "text": delta }));
            }
        }
    }
    if aborted {
        let _ = app.emit("ai-aborted", serde_json::json!({ "chatId": chat_id }));
    } else if let Some(error) = failed {
        let _ = app.emit(
            "ai-error",
            serde_json::json!({ "chatId": chat_id, "error": error, "partial": acc.content }),
        );
    } else {
        let mut payload = acc.finish();
        if let Some(map) = payload.as_object_mut() {
            map.insert("chatId".to_string(), serde_json::json!(chat_id));
        }
        let _ = app.emit("ai-done", payload);
    }
    cleanup_cancel(&chat_id);
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        // 自动更新：检查/下载走 updater 插件，装完重启走 process::relaunch。
        // 两端都只在 tauri.conf.json 的 plugins.updater 配上端点后才真正可用；
        // 未配置时前端调用会得到明确错误，不会静默成功。
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let data_dir = handle.path().app_data_dir()?;
            fs::create_dir_all(&data_dir)?;
            app.manage(AppState {
                core: Mutex::new(None),
                data_dir: data_dir.clone(),
                quitting: AtomicBool::new(false),
                core_stopped: AtomicBool::new(false),
                core_code: AtomicU8::new(0),
                stopping_by_request: AtomicBool::new(false),
                proxy_on: Mutex::new(false),
                tray_proxy: Mutex::new(None),
                tray: Mutex::new(None),
                api_key: Mutex::new(None),
                startup_error: Mutex::new(None),
            });
            // 核心的「退出进程」动作在壳内必须换成「报告已停止」：进程归壳所有，
            // 核心不得单方面 app.exit（用户点托盘退出才会真正退出）。
            let _ = SHELL_APP.set(handle.clone());
            orchestration::set_exit_hook(move |code| {
                let Some(app) = SHELL_APP.get() else { return };
                let state = app.state::<AppState>();
                state.core_code.store(code, Ordering::SeqCst);
                state.core_stopped.store(true, Ordering::SeqCst);
            });
            build_tray(&handle)?;
            match start_core(&handle) {
                Ok(core) => {
                    let state = handle.state::<AppState>();
                    *state.startup_error.lock().unwrap() = None;
                    *state.core.lock().unwrap() = Some(core);
                }
                Err(message) => {
                    // setup 里这次 emit 必然早于面板注册监听，事件必定丢失；把原因记下来交给
                    // watch_status 反复播报，面板挂载后才能看到失败原因和重试入口。
                    *handle.state::<AppState>().startup_error.lock().unwrap() = Some(message.clone());
                    let _ = handle.emit("core-failed", serde_json::json!({ "message": message }));
                }
            }
            watch_status(handle.clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let app = window.app_handle();
                if app.state::<AppState>().quitting.load(Ordering::SeqCst) {
                    // 已在退出流程里（托盘退出 / 上一次关闭触发的 quit）：不再拦截，让窗口正常销毁。
                    return;
                }
                // 关闭窗口即退出整个应用（含内嵌核心与 WorkBuddy 发布收尾），全平台一致：
                // 不再隐藏到托盘等用户「再去关一次后台」——驻留托盘时核心仍在跑，用户会以为已经停了。
                // 关停序列最坏数秒，因此先立即隐藏窗口再在后台线程走完退出，事件循环不被冻住。
                api.prevent_close();
                let _ = window.hide();
                let app = app.clone();
                thread::spawn(move || quit_app(&app));
            }
        })
        .invoke_handler(tauri::generate_handler![
            core_refresh,
            core_probe,
            core_import,
            core_system_proxy,
            core_shutdown,
            core_provider_status,
            core_set_provider_key,
            core_clear_provider_key,
            restart_core,
            core_running,
            data_dir_path,
            read_log,
            clear_log,
            open_external,
            ai_chat,
            ai_abort
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            match event {
                RunEvent::ExitRequested { .. } => {
                    // 必须是**有界**停止：自动更新的「重启」也走这条分支
                    // （process.relaunch → ExitRequested(RESTART_EXIT_CODE)），
                    // 在事件循环线程上直接跑 graceful_stop 会让窗口冻到停止序列走完（最坏十几秒），
                    // 用户看到的就不是「重启」而是「卡死」。
                    stop_core_bounded(app);
                    app.cleanup_before_exit();
                }
                #[cfg(target_os = "macos")]
                RunEvent::Reopen { .. } => show_main(app),
                _ => {}
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 外链命令是 WebView 之外唯一的「拉起系统浏览器」通道，只放行登记过的官方申请页：
    /// 任意 https、userinfo 视觉伪装、后缀拼接、带端口、非 https 都必须被拒绝，
    /// 否则被污染的面板可以把它当成任意 URL 外带通道。
    #[test]
    fn open_external_accepts_only_the_registered_official_hosts() {
        for url in [
            "https://www.modelscope.cn/my/access/token",
            "https://cloud.siliconflow.cn/account/ak",
            "https://console.cloud.tencent.com/tokenhub/apikey",
            "https://open.bigmodel.cn/usercenter/apikeys",
        ] {
            assert_eq!(external_url_rejection(url), None, "{url} 是名单内的官方页，必须放行");
        }
        for url in [
            "https://evil.tld/",
            "http://www.modelscope.cn/my/access/token",
            "https://www.modelscope.cn@evil.tld/",
            "https://www.modelscope.cn.evil.tld/token",
            "https://www.modelscope.cn:8443/my/access/token",
            "https://www.modelscope.cn\\@evil.tld/",
            "https://open.bigmodel.cn/usercenter/apikeys\nhttps://evil.tld",
            "https://",
            "",
            "file:///etc/passwd",
        ] {
            assert!(
                external_url_rejection(url).is_some(),
                "{url} 不得被当作官方申请页打开"
            );
        }
    }

    /// 临时日志文件；Drop 时清理，避免测试往磁盘留垃圾。
    struct TempLog(PathBuf);

    impl TempLog {
        fn new(name: &str, content: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("wb-bridge-log-{}-{name}.log", std::process::id()));
            let mut file = fs::File::create(&path).expect("创建临时日志");
            file.write_all(content.as_bytes()).expect("写入临时日志");
            Self(path)
        }
    }

    impl Drop for TempLog {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn absent_log_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wb-bridge-log-{}-{name}-absent.log", std::process::id()))
    }

    /// 核心还没写过日志（文件不存在）时按空日志处理：不报错、bytes 为 0。
    #[test]
    fn missing_log_reads_as_empty_without_error() {
        let value = read_log_tail(&absent_log_path("missing"), 1024, 10).expect("缺失日志不得报错");
        assert_eq!(value["text"], "");
        assert_eq!(value["truncated"], false);
        assert_eq!(value["bytes"], 0);
    }

    /// 小文件整份返回：bytes 是文件真实字节数（不是返回文本长度），truncated 为 false。
    #[test]
    fn small_log_is_returned_whole() {
        let log = TempLog::new("small", "line one\nline two\n");
        let value = read_log_tail(&log.0, 1024, 10).expect("读取小日志");
        assert_eq!(value["text"], "line one\nline two");
        assert_eq!(value["truncated"], false);
        assert_eq!(value["bytes"], 18);
    }

    /// 超过字节上限：只回尾部，且丢掉跨字节起点被截断的半行，保证首行是完整日志行。
    #[test]
    fn oversized_log_keeps_tail_and_drops_the_split_first_line() {
        let content = "header line that will be cut\nkeep me\nand me\n";
        let log = TempLog::new("bytes", content);
        let value = read_log_tail(&log.0, 20, 100).expect("读取超长日志");
        assert_eq!(value["truncated"], true);
        assert_eq!(value["text"], "keep me\nand me");
        assert_eq!(value["bytes"], content.len());
    }

    /// 超过行数上限：只保留最后 max_lines 行，并如实报告 truncated。
    #[test]
    fn oversized_log_keeps_only_the_last_lines() {
        let log = TempLog::new("lines", "a\nb\nc\nd\n");
        let value = read_log_tail(&log.0, 1024, 2).expect("读取多行日志");
        assert_eq!(value["truncated"], true);
        assert_eq!(value["text"], "c\nd");
    }

    /// 清空有内容的日志：文件被截断为空，且随后的 read_log_tail 读到空日志。
    #[test]
    fn clear_log_truncates_the_existing_log_file() {
        let log = TempLog::new("clear", "line one\nline two\n");
        let value = clear_log_file(&log.0).expect("清空日志");
        assert_eq!(value["cleared"], true);
        let after = read_log_tail(&log.0, 1024, 10).expect("清空后读取日志");
        assert_eq!(after["text"], "");
        assert_eq!(after["bytes"], 0);
    }

    /// 文件不存在（核心还没写过日志）时清空按 no-op 处理：不报错、cleared 为 false。
    #[test]
    fn clear_log_missing_file_is_a_noop_without_error() {
        let value = clear_log_file(&absent_log_path("clear-missing")).expect("缺失日志不得报错");
        assert_eq!(value["cleared"], false);
    }

    /// 壳的动作表必须与核心的契约表逐项一致：任一侧改名或漏项，面板动作会在回环 HTTP 上 404，
    /// 而面板只会显示一句「请求失败」。
    #[test]
    fn shell_action_routes_match_the_core_contract() {
        use wbbridge_core::server::ACTION_ROUTES;
        let mut shell: Vec<(&str, &str, &str)> =
            ADMIN_ROUTES.iter().map(|(name, route)| (*name, "POST", *route)).collect();
        let mut core: Vec<(&str, &str, &str)> = ACTION_ROUTES.to_vec();
        shell.sort_unstable();
        core.sort_unstable();
        assert_eq!(shell, core);
    }

    /// AI 端点白名单：https 一律放行，http 只认本机回环；前缀伪装（127.0.0.1.evil.com）
    /// 与非 http(s) 协议一律拒绝。
    #[test]
    fn ai_base_url_allows_https_and_loopback_http_only() {
        assert_eq!(
            normalize_ai_base_url(" https://api.deepseek.com/v1/ ").unwrap(),
            "https://api.deepseek.com/v1"
        );
        assert_eq!(
            normalize_ai_base_url("http://127.0.0.1:11434/v1").unwrap(),
            "http://127.0.0.1:11434/v1"
        );
        assert_eq!(
            normalize_ai_base_url("http://localhost:8000/v1").unwrap(),
            "http://localhost:8000/v1"
        );
        assert_eq!(
            normalize_ai_base_url("http://[::1]:8080/v1").unwrap(),
            "http://[::1]:8080/v1"
        );
        assert!(normalize_ai_base_url("http://api.deepseek.com/v1").is_err(), "非本机 http 拒绝");
        assert!(normalize_ai_base_url("http://127.0.0.1.evil.com/v1").is_err(), "前缀伪装拒绝");
        assert!(normalize_ai_base_url("http://localhost.evil.com/v1").is_err(), "前缀伪装拒绝");
        assert!(normalize_ai_base_url("ftp://api.deepseek.com").is_err());
        assert!(normalize_ai_base_url("api.deepseek.com/v1").is_err());
        assert!(normalize_ai_base_url("").is_err());
        assert!(normalize_ai_base_url("https://").is_err(), "缺主机名");
    }

    /// SSE 累积：content 增量拼接、tool_calls 的 id/name 只取首片、arguments 按分片拼接，
    /// finish_reason 以最后一个非空值为准；返回值是本次的文本增量（供节流推送）。
    #[test]
    fn ai_stream_acc_absorbs_content_and_tool_calls() {
        let mut acc = AiStreamAcc::default();
        assert_eq!(acc.absorb(&serde_json::json!({ "choices": [] })), "");
        let delta = acc.absorb(&serde_json::json!({
            "choices": [{ "delta": { "content": "你好" } }]
        }));
        assert_eq!(delta, "你好");
        let delta = acc.absorb(&serde_json::json!({
            "choices": [{ "delta": { "content": "，世界" } }]
        }));
        assert_eq!(delta, "，世界");
        // 首片带 id/name，arguments 分两片到达。
        acc.absorb(&serde_json::json!({
            "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "id": "call_1", "function": { "name": "get_status", "arguments": "{\"a\"" } }] } }]
        }));
        acc.absorb(&serde_json::json!({
            "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": ":1}" } }] } }]
        }));
        // 第二个工具 + finish_reason。
        acc.absorb(&serde_json::json!({
            "choices": [{ "index": 0, "finish_reason": "tool_calls", "delta": { "tool_calls": [{ "index": 1, "id": "call_2", "function": { "name": "read_log", "arguments": "{}" } }] } }]
        }));
        let finished = acc.finish();
        assert_eq!(finished["content"], "你好，世界");
        assert_eq!(finished["finishReason"], "tool_calls");
        let calls = finished["toolCalls"].as_array().expect("toolCalls 应为数组");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["name"], "get_status");
        assert_eq!(calls[0]["arguments"], "{\"a\":1}");
        assert_eq!(calls[1]["name"], "read_log");
    }

    /// usage-only chunk（无 choices）不产出增量；只有 index 没有 name 的空槽不进最终数组。
    #[test]
    fn ai_stream_acc_ignores_usage_only_and_nameless_slots() {
        let mut acc = AiStreamAcc::default();
        assert_eq!(
            acc.absorb(&serde_json::json!({ "usage": { "total_tokens": 42 } })),
            ""
        );
        acc.absorb(&serde_json::json!({
            "choices": [{ "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "{}" } }] } }]
        }));
        let finished = acc.finish();
        assert_eq!(finished["content"], "");
        assert_eq!(finished["toolCalls"].as_array().map(Vec::len), Some(0));
    }

    /// 流中错误帧必须被识别为失败：核心在响应头已发出后只能以 `data: {"error":{...}}`
    /// 补发失败且没有 `[DONE]`（`server.rs::StreamJob` 失败路径）；这类帧没有 `choices`，
    /// 若不单独识别，面板会拿到「空内容 + 无工具调用」的成功——表现为转圈结束后气泡
    /// 一片空白、不报任何原因（WB · auto 落到慢/坏免费模型时的真实症状）。
    #[test]
    fn ai_stream_error_frame_is_surfaced_not_swallowed() {
        // 核心 StreamJob 失败路径的原样帧（BackendError::error_value 的形状）。
        let frame = serde_json::json!({
            "error": { "message": "Model request timed out", "type": "upstream_error", "code": "upstream_error" }
        });
        assert_eq!(
            sse_error_of(&frame),
            Some("AI 端点在流中返回错误：Model request timed out".to_string())
        );
        // 正常帧（choices / usage-only）不得误判为错误。
        assert_eq!(sse_error_of(&serde_json::json!({ "choices": [{ "delta": { "content": "hi" } }] })), None);
        assert_eq!(sse_error_of(&serde_json::json!({ "usage": { "total_tokens": 1 } })), None);
        // error 为字符串的端点形态同样要识别；null / 空 error 不算错误。
        assert_eq!(
            sse_error_of(&serde_json::json!({ "error": "boom" })),
            Some("AI 端点在流中返回错误：boom".to_string())
        );
        assert_eq!(sse_error_of(&serde_json::json!({ "error": null })), None);
        assert_eq!(sse_error_of(&serde_json::json!({ "error": "" })), None);
        // message 缺失时退化到 code，再缺退化成整段 JSON 的截断。
        let code_only = serde_json::json!({ "error": { "type": "upstream_error", "code": "model_failed" } });
        assert_eq!(
            sse_error_of(&code_only),
            Some("AI 端点在流中返回错误：model_failed".to_string())
        );
    }

    /// 错误响应体截断：压掉换行、封顶 200 字符，超长加省略号。
    #[test]
    fn ai_error_snippet_collapses_and_caps() {
        assert_eq!(snippet_of("  line one\nline two  "), "line one line two");
        assert_eq!(snippet_of(""), "");
        let long = snippet_of(&"x".repeat(500));
        assert_eq!(long.chars().count(), 201);
        assert!(long.ends_with('…'));
    }

    /// 请求形态：camelCase 入参可反解，缺省项（selfHosted / tools / toolChoice）回落空值。
    /// 这条同时钉住前端 `src/core/ai.js` 与壳的字段拼写 —— 改名必须两侧同步。
    #[test]
    fn ai_request_accepts_the_panel_shape_and_defaults() {
        let request: AiChatRequest = serde_json::from_value(serde_json::json!({
            "chatId": "ai-1",
            "baseUrl": "https://api.deepseek.com/v1",
            "apiKey": "sk-x",
            "model": "deepseek-v4-flash",
            "messages": [{ "role": "user", "content": "现在有几个模型可用" }],
        }))
        .expect("面板形态的请求体必须可解");
        assert!(!request.self_hosted);
        assert!(request.tools.is_empty());
        assert_eq!(request.tool_choice, None);
        assert_eq!(
            validate_ai_request(&request).unwrap(),
            "https://api.deepseek.com/v1"
        );
    }

    /// 体积与形态闸门：空 messages / 超长单条 / 超量历史 / 超量工具都要在起线程之前挡下；
    /// 自托管模式跳过 Base URL 校验（端点由壳自己解析，面板无权指定）。
    #[test]
    fn ai_request_guards_shape_sizes_and_self_hosted_skips_base_url() {
        let base = |extra: serde_json::Value| {
            let mut request = serde_json::json!({
                "chatId": "ai-1",
                "baseUrl": "https://api.deepseek.com/v1",
                "model": "m",
                "messages": [{ "role": "user", "content": "hi" }],
            });
            if let (Some(map), Some(given)) = (request.as_object_mut(), extra.as_object()) {
                for (key, value) in given {
                    map.insert(key.clone(), value.clone());
                }
            }
            serde_json::from_value::<AiChatRequest>(request).expect("形态合法")
        };
        assert!(validate_ai_request(&base(serde_json::json!({ "messages": [] }))).is_err());
        let too_long = "x".repeat(AI_MAX_MESSAGE_CHARS + 1);
        assert!(validate_ai_request(&base(serde_json::json!({
            "messages": [{ "role": "user", "content": too_long }]
        })))
        .is_err());
        let many: Vec<Value> = (0..=AI_MAX_MESSAGES)
            .map(|_| serde_json::json!({ "role": "user", "content": "hi" }))
            .collect();
        assert!(validate_ai_request(&base(serde_json::json!({ "messages": many }))).is_err());
        let tools: Vec<Value> = (0..=AI_MAX_TOOLS)
            .map(|index| serde_json::json!({ "type": "function", "index": index }))
            .collect();
        assert!(validate_ai_request(&base(serde_json::json!({ "tools": tools }))).is_err());
        // 非法端点：非自托管必须被白名单拒绝。
        assert!(validate_ai_request(&base(serde_json::json!({ "baseUrl": "http://evil.test/v1" }))).is_err());
        // 自托管：baseUrl 完全忽略，缺省即通过。
        let self_hosted = base(serde_json::json!({
            "selfHosted": true,
            "baseUrl": "http://evil.test/v1",
        }));
        assert_eq!(validate_ai_request(&self_hosted).unwrap(), "");
    }

    /// 请求体：不带 tools 时保持旧形态（老端点不认识 tools 字段）；带 tools 才补 tool_choice，
    /// 面板显式给的 tool_choice（例如 "required"）不被覆盖成 auto。
    #[test]
    fn ai_body_declares_tools_only_when_present() {
        let plain: AiChatRequest = serde_json::from_value(serde_json::json!({
            "chatId": "c", "baseUrl": "https://x.test/v1", "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
        }))
        .unwrap();
        let body = build_ai_body(&plain);
        assert_eq!(body["stream"], true);
        assert!(body.get("tools").is_none(), "无工具时不得发送 tools 键");
        assert!(body.get("tool_choice").is_none());

        let with_tools: AiChatRequest = serde_json::from_value(serde_json::json!({
            "chatId": "c", "baseUrl": "https://x.test/v1", "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "type": "function", "function": { "name": "get_service_status" } }],
        }))
        .unwrap();
        let body = build_ai_body(&with_tools);
        assert_eq!(body["tools"].as_array().map(Vec::len), Some(1));
        assert_eq!(body["tool_choice"], "auto");

        let forced: AiChatRequest = serde_json::from_value(serde_json::json!({
            "chatId": "c", "baseUrl": "https://x.test/v1", "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "tools": [{ "type": "function", "function": { "name": "probe_models" } }],
            "toolChoice": "required",
        }))
        .unwrap();
        assert_eq!(build_ai_body(&forced)["tool_choice"], "required");
    }

    /// 自托管端点只指向回环地址（端口由壳自己给出，面板不参与拼接）。
    #[test]
    fn self_hosted_base_url_is_loopback_only() {
        assert_eq!(self_hosted_base_url(41980), "http://127.0.0.1:41980/v1");
        assert_eq!(
            resolve_self_hosted_without_core_is_refused(),
            "核心服务未运行，本机模型不可用；请改用外部端点"
        );
    }

    /// 没有运行中的核心时自托管必须失败，不得退化成「猜一个端口」或任意本机地址。
    /// 没有 AppHandle 就调不到 resolve_self_hosted，这里复现它的前半段判据并钉住文案。
    fn resolve_self_hosted_without_core_is_refused() -> String {
        let state = idle_state();
        let port = {
            let guard = state.core.lock().unwrap();
            guard.as_ref().map(|core| core.port)
        };
        match port {
            Some(port) => format!("{}/chat/completions", self_hosted_base_url(port)),
            None => "核心服务未运行，本机模型不可用；请改用外部端点".to_string(),
        }
    }

    fn idle_state() -> AppState {
        AppState {
            core: Mutex::new(None),
            data_dir: PathBuf::new(),
            quitting: AtomicBool::new(false),
            core_stopped: AtomicBool::new(false),
            core_code: AtomicU8::new(0),
            stopping_by_request: AtomicBool::new(false),
            proxy_on: Mutex::new(false),
            tray_proxy: Mutex::new(None),
            tray: Mutex::new(None),
            api_key: Mutex::new(None),
            startup_error: Mutex::new(None),
        }
    }

    /// 关窗即退出后，「点关闭」「托盘点退出」「`RunEvent::ExitRequested`」会在同一进程里前后触发
    /// `graceful_stop`；`quitting` 的 swap 是唯一的幂等保证。第二次必须直接返回且不碰状态。
    #[test]
    fn repeated_quit_requests_stop_only_once() {
        let state = idle_state();
        assert!(!state.quitting.load(Ordering::SeqCst));
        graceful_stop(&state);
        assert!(state.quitting.load(Ordering::SeqCst), "首次关停必须置位 quitting");
        // 再次触发不得 panic、不得改动已取空的槽位。
        graceful_stop(&state);
        assert!(state.core.lock().unwrap().is_none());
    }

    /// 用户主动停止（关窗或托盘退出）不是故障：即使核心已报告结束，也不能播报「核心服务已退出」，
    /// 否则关窗收尾的几秒里面板会弹出一条假的故障与「重试」入口。
    #[test]
    fn a_quit_requested_shutdown_is_not_reported_as_failure() {
        let state = idle_state();
        state.core_stopped.store(true, Ordering::SeqCst);
        state.stopping_by_request.store(true, Ordering::SeqCst);
        assert!(core_exit(&state).is_none(), "主动关停不得算作故障退出");
        assert!(service_down(&state).is_none(), "主动关停期间不得推送故障态");
        // 对照：真正的崩溃仍必须被识别，否则上面两条断言会因为永远返回 None 而失去意义。
        state.stopping_by_request.store(false, Ordering::SeqCst);
        assert!(core_exit(&state).is_some());
    }

    /// 故障播报只认真正的故障：核心在跑不报；真崩溃必须报「已退出」；**关窗退出的收尾期**
    /// （core 槽已被取走、主动停止标志在位）不得谎报故障，否则面板会在退出过程中弹出一条假的
    /// 「核心服务已退出 + 重试」；启动失败则必须报出原因且不能说成「已退出」。
    #[test]
    fn service_down_reports_only_real_failures() {
        let running = idle_state();
        *running.core.lock().unwrap() = Some(RunningCore {
            runtime: tokio::runtime::Runtime::new().expect("测试用运行时"),
            port: 41980,
        });
        assert!(service_down(&running).is_none(), "核心在跑时不得报故障");

        // 同一份状态改成「崩溃」口径：exit hook 置位且非主动停止。
        running.stopping_by_request.store(false, Ordering::SeqCst);
        running.core_stopped.store(true, Ordering::SeqCst);
        let (message, by_exit) = service_down(&running).expect("崩溃必须报故障");
        assert!(by_exit);
        assert!(message.contains("核心服务已退出"), "{message}");

        // 关窗退出的收尾期：没有核心、核心也已结束，但这是用户主动停的。
        let shutting = idle_state();
        shutting.core_stopped.store(true, Ordering::SeqCst);
        shutting.stopping_by_request.store(true, Ordering::SeqCst);
        assert!(service_down(&shutting).is_none(), "主动关停期间不得推送故障态");

        // 启动失败：没有核心在跑，原因来自 startup_error，口径不得与「已退出」混用。
        let failed = idle_state();
        *failed.startup_error.lock().unwrap() = Some("核心启动失败：端口不可用".to_string());
        let (message, by_exit) = service_down(&failed).expect("启动失败必须报故障");
        assert!(!by_exit, "启动失败不是「核心已退出」");
        assert_eq!(message, "核心启动失败：端口不可用");
    }

    /// status.json 快路径的判定：只有在「拿得到 mtime 且 (mtime, 长度) 与上次一致」时才允许跳过。
    /// 重点保护两件事：mtime 不可得时必须回落读内容（否则长度不变的改写被永久跳过、面板冻结），
    /// 以及 mtime 或长度任一变化都要重读（重启后内容可能与故障前逐字节相同，靠的就是这道判定）。
    #[test]
    fn status_read_needed_only_skips_when_mtime_is_known() {
        use std::time::UNIX_EPOCH;
        let first = (UNIX_EPOCH, 8u64);
        let later = (UNIX_EPOCH + std::time::Duration::from_secs(1), 8u64);
        let resized = (UNIX_EPOCH, 9u64);

        assert!(
            status_read_needed(None, Some(first)),
            "文件系统不暴露 mtime 时不得启用快路径"
        );
        assert!(
            status_read_needed(None, None),
            "首轮且拿不到 mtime 时必须读"
        );
        assert!(!status_read_needed(Some(first), Some(first)), "mtime 与长度都没变才允许跳过");
        assert!(status_read_needed(Some(later), Some(first)), "mtime 变了必须重读");
        assert!(status_read_needed(Some(resized), Some(first)), "长度变了必须重读");
        assert!(status_read_needed(Some(first), None), "没有上次记录时必须读（含恢复运行后重置 stamp 的情形）");
    }
}
