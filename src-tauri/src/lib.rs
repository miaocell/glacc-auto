// 步 4：设置页与弹窗 IPC 接线。
// 步 1、2 已完成协议/凭据/登录/领取/调度/日志的后端闭环，步 3 接入了首页。
// 本步把设置页与登录弹窗需要的能力暴露给前端：
// - 设置读写（settings.json + schedule.json，原子落盘、读取即夹紧）
// - 短信登录两步（发码 / 验码登录），账号上限 3 在发码前与落盘前各拦一次
// - 显示缩放（WebView2 缩放 + 窗口同比尺寸）
// - 退出单个账号（供通用确认框使用）
//
// 步 5 新增三个只读/文件类命令，服务「数据与日志」页：
// - read_logs：读当前日志文件尾部，按行切出时间戳与等级
// - export_credentials / import_credentials：系统文件选择框 + 复用 store 的导入导出
//   两者都在主线程之外执行（阻塞式文件框不允许在主线程调用），
//   文件选择框通过 tauri-plugin-dialog 在 Rust 侧拉起，不暴露插件命令给前端。
//
// 步 6 新增（打包与定时任务收口）：
// - apply_schedule：真正向 Windows 注册/注销 glacc-auto-claim 计划任务（schtasks）
// - run_scheduled：命令行带 --scheduled 时不建窗口，静默跑一轮领取后退出
//   跨进程互斥（glacc::instance）保证"计划任务实例"与"界面实例"不会同时领取
// - 启动时后台对齐计划任务定义（任务缺失或 exe/时刻不符则补注册）

pub mod glacc;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde::{Deserialize, Serialize};
use tauri::State;
use tauri_plugin_dialog::DialogExt;

/// CREATE_NO_WINDOW：后台子进程不分配控制台窗口（否则开浏览器/调 schtasks 会闪黑框）。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

use glacc::auth;
use glacc::claim::{describe_reason, mask_phone, new_serial_gate, score_to_minutes, ClaimEvent, Reporter, SerialGate};
use glacc::credentials::Credentials;
use glacc::diag;
use glacc::game::{GameApi, GameClient};
use glacc::history;
use glacc::instance::ClaimLock;
use glacc::notify;
use glacc::schedule::{self, ScheduleConfig};
use glacc::session::Session;
use glacc::settings::AppSettings;
use glacc::store;
use glacc::sweep::{
    plan_from_store, AccountSweep, LiveClaimant, SweepEntry, SweepEvent, SweepOutcome, SweepReporter,
    SweepState,
};

/// 首页共享状态：一轮领取的运行态与上一轮结果。
#[derive(Default)]
struct Runtime {
    running: bool,
    status: String,
    error: bool,
    last: Option<SweepOutcome>,
    /// 本轮实时进度板（running 期间由领取线程写，界面每秒读）。
    live: SharedBoard,
}

/// 实时进度板里的一行。与 AccountProgressDto 同形，但可以独立克隆给多个回调持有。
#[derive(Debug, Clone)]
struct LiveEntry {
    key: String,
    label: String,
    /// pending | running | done | failed | skipped
    state: &'static str,
    claimed: i32,
    total: i32,
    added_minutes: i32,
    balance_minutes: f64,
    message: Option<String>,
}

/// 领取线程写的实时进度板。轮次级快照（Runtime::last）只在整轮结束后才有值，
/// 逐号开始/结束、逐阶段 +N 分钟这些中间态只能靠它送到界面。
#[derive(Default)]
struct LiveBoard {
    entries: Vec<LiveEntry>,
    /// 最近一条领取事件文案，界面状态栏直接显示（例如"点广告得时长·阶段一：+2 分钟"）
    note: String,
    /// 本次运行的滚动事件流（带头时间戳），首页"运行状态"卡片逐行滚动展示。
    /// 无上限会随长时间运行无限涨，这里只留最近 LIVE_LOG_MAX 条。
    log: Vec<String>,
}

/// 运行状态卡片保留的事件条数上限。
const LIVE_LOG_MAX: usize = 200;

/// 追加一行运行事件：前缀 HH:mm:ss，超出上限丢最旧的。
fn push_live_log(board: &mut LiveBoard, message: impl AsRef<str>) {
    let stamp = diag::local_timestamp();
    // 固定格式 "yyyy-MM-dd HH:mm:ss.fff"，取时分秒那一段（纯 ASCII，切片安全）
    let short = stamp.get(11..19).unwrap_or(&stamp).to_string();
    board.log.push(format!("{short}  {}", message.as_ref()));
    if board.log.len() > LIVE_LOG_MAX {
        let drop = board.log.len() - LIVE_LOG_MAX;
        board.log.drain(..drop);
    }
}

type SharedBoard = Arc<Mutex<LiveBoard>>;

fn lock_board(board: &Mutex<LiveBoard>) -> std::sync::MutexGuard<'_, LiveBoard> {
    board.lock().unwrap_or_else(|e| e.into_inner())
}

struct HomeState {
    runtime: Arc<Mutex<Runtime>>,
    /// 与领取模块共用的全局串行门闸：同一时刻只允许一个领取任务。
    gate: SerialGate,
    /// 登录链门闸：发码与登录是阻塞网络调用，串行化避免连点造成并发请求
    /// （同一 pending_phone 上并发发码会互相覆盖 verification_id）。
    auth_gate: SerialGate,
}

impl Default for HomeState {
    fn default() -> Self {
        Self {
            runtime: Arc::new(Mutex::new(Runtime::default())),
            gate: new_serial_gate(),
            auth_gate: new_serial_gate(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountDto {
    key: String,
    label: String,
    is_current: bool,
    last_used: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountProgressDto {
    key: String,
    label: String,
    /// pending | running | done | failed | skipped
    state: &'static str,
    claimed: i32,
    total: i32,
    added_minutes: i32,
    balance_minutes: f64,
    message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimProgressDto {
    running: bool,
    claimed: i32,
    total: i32,
    added_minutes: i32,
    balance_minutes: f64,
    status_text: String,
    status_is_error: bool,
    accounts: Vec<AccountProgressDto>,
    /// 本次运行的滚动事件流（旧到新），首页"运行状态"卡片直接渲染
    events: Vec<String>,
}

fn state_name(state: SweepState) -> &'static str {
    match state {
        SweepState::Pending => "pending",
        SweepState::Running => "running",
        SweepState::Done => "done",
        SweepState::Failed => "failed",
        SweepState::Skipped => "skipped",
    }
}

fn entry_dto(entry: &SweepEntry) -> AccountProgressDto {
    AccountProgressDto {
        key: entry.key.clone(),
        label: entry.label.clone(),
        state: state_name(entry.state),
        claimed: entry.claimed,
        total: entry.total,
        added_minutes: entry.added_minutes,
        balance_minutes: entry.balance_minutes,
        message: entry.message.clone(),
    }
}

fn progress_dto(runtime: &Runtime) -> ClaimProgressDto {
    if runtime.running {
        return live_progress_dto(runtime);
    }
    let (claimed, total, added, balance, accounts) = match &runtime.last {
        Some(outcome) => (
            outcome.entries.iter().map(|e| e.claimed).sum(),
            outcome.entries.iter().map(|e| e.total).sum(),
            outcome.total_added_minutes,
            outcome
                .entries
                .iter()
                .map(|e| e.balance_minutes)
                .fold(0.0_f64, f64::max),
            outcome.entries.iter().map(entry_dto).collect(),
        ),
        None => (0, 0, 0, 0.0, Vec::new()),
    };
    ClaimProgressDto {
        running: runtime.running,
        claimed,
        total,
        added_minutes: added,
        balance_minutes: balance,
        status_text: runtime.status.clone(),
        status_is_error: runtime.error,
        accounts,
        // 整轮结束后进度板仍保留，运行状态卡片继续显示上一轮的事件流
        events: lock_board(&runtime.live).log.clone(),
    }
}

/// 运行中的进度快照：数据来自领取线程写的实时进度板。
/// 这样界面能实时看到"正在领哪个号、这个号进行到第几次"，而不是整轮结束才有结果。
fn live_progress_dto(runtime: &Runtime) -> ClaimProgressDto {
    let board = lock_board(&runtime.live);
    let accounts: Vec<AccountProgressDto> = board
        .entries
        .iter()
        .map(|e| AccountProgressDto {
            key: e.key.clone(),
            label: e.label.clone(),
            state: e.state,
            claimed: e.claimed,
            total: e.total,
            added_minutes: e.added_minutes,
            balance_minutes: e.balance_minutes,
            message: e.message.clone(),
        })
        .collect();
    let claimed = accounts.iter().map(|e| e.claimed).sum();
    let total = accounts.iter().map(|e| e.total).sum();
    let added = accounts.iter().map(|e| e.added_minutes).sum();
    let balance = accounts
        .iter()
        .map(|e| e.balance_minutes)
        .fold(0.0_f64, f64::max);
    ClaimProgressDto {
        running: true,
        claimed,
        total,
        added_minutes: added,
        balance_minutes: balance,
        status_text: if board.note.is_empty() {
            runtime.status.clone()
        } else {
            board.note.clone()
        },
        status_is_error: runtime.error,
        accounts,
        events: board.log.clone(),
    }
}

fn lock_runtime(runtime: &Mutex<Runtime>) -> std::sync::MutexGuard<'_, Runtime> {
    // 领取线程 panic 时锁可能中毒：取回内层数据继续展示，不让界面永久卡在"领取中"。
    runtime.lock().unwrap_or_else(|e| e.into_inner())
}

// ── 窗口控制（自绘标题栏用）──
// 走自定义命令而不是 window 插件：插件命令受 capabilities ACL 约束，本步不动权限文件。
// 前端在标题栏的 pointerdown 里 stopPropagation，避免 Tauri 内置 drag 脚本重复触发拖拽。

#[tauri::command]
fn window_drag(window: tauri::WebviewWindow) -> Result<(), String> {
    window.start_dragging().map_err(|e| e.to_string())
}

#[tauri::command]
fn window_minimize(window: tauri::WebviewWindow) -> Result<(), String> {
    window.minimize().map_err(|e| e.to_string())
}

#[tauri::command]
fn window_toggle_maximize(window: tauri::WebviewWindow) -> Result<(), String> {
    let maximized = window.is_maximized().map_err(|e| e.to_string())?;
    if maximized {
        window.unmaximize().map_err(|e| e.to_string())
    } else {
        window.maximize().map_err(|e| e.to_string())
    }
}

#[tauri::command]
fn window_close(window: tauri::WebviewWindow) -> Result<(), String> {
    window.close().map_err(|e| e.to_string())
}

/// 显示窗口（步 7）：窗口在 tauri.conf.json 里以 visible:false 创建，
/// 前端 onMount 里首帧渲染完成后调它显示，避免小窗白屏闪烁。
/// show() 是幂等的，重复调用无害（也兜住启动兜底线程的调用）。
#[tauri::command]
fn window_show(window: tauri::WebviewWindow) -> Result<(), String> {
    window.show().map_err(|e| e.to_string())
}

/// 用系统默认浏览器打开一个 https 链接（步 8：设置页底部 GitHub 图标用）。
/// 未装 opener 插件，直接交给系统外壳（Windows 的 start 命令），零新增依赖；
/// 只放行 https，避免把任意字符串喂给命令行。
#[tauri::command]
fn open_external(url: String) -> Result<(), String> {
    let url = url.trim();
    if !url.starts_with("https://") {
        return Err("只允许打开 https 链接".to_string());
    }
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("无法打开浏览器：{e}"))
}

// ── 账号 ──

fn accounts_dto() -> Vec<AccountDto> {
    store::list_accounts()
        .into_iter()
        .enumerate()
        .map(|(i, account)| AccountDto {
            label: mask_phone(&account.phone_digits),
            key: account.key,
            is_current: i == 0,
            last_used: account.last_used,
        })
        .collect()
}

/// 已登录账号列表（最近使用倒序，上限由 store 内置的 3 个截断）。
#[tauri::command]
fn get_accounts() -> Vec<AccountDto> {
    accounts_dto()
}

/// 退出单个账号：删除其凭据文件（旧版 ConfirmLogout 的等价动作）。
/// key 为空或缺省时取最近使用的账号；返回删除后的账号列表。
#[tauri::command]
fn logout_account(key: Option<String>) -> Result<Vec<AccountDto>, String> {
    let target = match key.filter(|k| !k.is_empty()) {
        Some(key) => key,
        None => {
            let latest = store::list_accounts()
                .first()
                .map(|a| a.key.clone())
                .ok_or_else(|| "没有可退出的账号".to_string())?;
            latest
        }
    };
    store::remove(&target);
    diag::info("已退出登录：账号凭据已删除");
    Ok(accounts_dto())
}

/// 切换当前激活账号：把目标账号标记为"最近使用"（当前账号 = 最近使用的账号）。
/// 只触碰凭据文件的修改时间，不改任何登录态与领取逻辑。
#[tauri::command]
fn set_active_account(key: String) -> Result<Vec<AccountDto>, String> {
    if !store::list_accounts().iter().any(|a| a.key == key) {
        return Err("该账号不存在".to_string());
    }
    store::touch(&key)?;
    diag::info("已切换当前账号");
    Ok(accounts_dto())
}

// ── 设置 ──

/// 设置页契约。字段名与前端 camelCase 一一对应；
/// 定时开关来自 schedule.json，其余来自 settings.json。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsDto {
    scheduled_enabled: bool,
    scheduled_time: String,
    server_key: String,
    interval_min_sec: i32,
    interval_max_sec: i32,
    network_retry_count: i32,
    /// system | light | dark
    theme: String,
    /// 关于行版本号；保存时忽略
    #[serde(default)]
    version: String,
}

impl SettingsDto {
    fn from_sources() -> Self {
        let settings = AppSettings::load();
        let schedule = ScheduleConfig::load();
        Self {
            scheduled_enabled: schedule.enabled,
            // 时刻以 settings.json 为准；apply_schedule 完成后会把系统任务的真实时刻写回两处文件
            scheduled_time: settings.scheduled_time.clone(),
            server_key: settings.server_key,
            interval_min_sec: settings.interval_min_sec,
            interval_max_sec: settings.interval_max_sec,
            network_retry_count: settings.network_retry_count,
            theme: settings.theme,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

/// 读取当前设置（已按落盘口径夹紧）。
#[tauri::command]
fn get_settings() -> SettingsDto {
    SettingsDto::from_sources()
}

/// 保存设置：settings.json 与 schedule.json 都走临时文件 + 改名。
/// 返回夹紧后的实际值，前端据此回写控件（与旧版失焦校验写回控件同语义）。
#[tauri::command]
fn save_settings(input: SettingsDto) -> Result<SettingsDto, String> {
    // 领取间隔：旧版 ViewModel 启动校正是"min > max 时取 max"，这里保持同一取舍
    let mut interval_min = input.interval_min_sec.clamp(1, 3600);
    let interval_max = input.interval_max_sec.clamp(1, 3600);
    if interval_min > interval_max {
        interval_min = interval_max;
    }

    let mut settings = AppSettings {
        scheduled_time: input.scheduled_time.trim().to_string(),
        server_key: input.server_key,
        interval_min_sec: interval_min,
        interval_max_sec: interval_max,
        network_retry_count: input.network_retry_count.clamp(0, 10),
        theme: input.theme,
    };
    settings.normalize();
    settings.save();

    // 定时开关与时刻：与 settings.json 的时刻保持同步，避免两处各存一份而漂移
    let mut schedule = ScheduleConfig {
        enabled: input.scheduled_enabled,
        time: settings.scheduled_time.clone(),
    };
    schedule.normalize();
    schedule.save();

    Ok(SettingsDto {
        scheduled_enabled: schedule.enabled,
        scheduled_time: schedule.time,
        server_key: settings.server_key,
        interval_min_sec: settings.interval_min_sec,
        interval_max_sec: settings.interval_max_sec,
        network_retry_count: settings.network_retry_count,
        theme: settings.theme,
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
}

// ── 短信登录 ──

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SmsCodeDto {
    /// 脱敏手机号（验证码已发送至 …）
    phone_label: String,
    /// 前端重发倒计时秒数（与旧版一致 60 秒）
    resend_seconds: i32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginResultDto {
    key: String,
    label: String,
}

/// 账号上限文案（发码前与落盘前各用一条，措辞对齐旧版两个拦截点）。
fn cap_message(for_send: bool) -> String {
    if for_send {
        format!(
            "最多只能保存 {} 个账号（家庭自用上限），请先删除一个不再使用的账号，或改用已有账号登录",
            store::MAX_ACCOUNTS
        )
    } else {
        format!(
            "最多只能保存 {} 个账号（家庭自用上限），请先删除一个不再使用的账号后重新登录",
            store::MAX_ACCOUNTS
        )
    }
}

/// 发码（阻塞网络）。账号上限在发码前拦：验证码只有 5 分钟有效，
/// 等用户输完才拒绝等于白折腾一次（与旧版 SendCodeAsync 的拦截位置一致）。
fn send_sms_blocking(phone: &str) -> Result<SmsCodeDto, String> {
    let digits = Credentials::phone_digits(phone);
    if digits.chars().count() == 11 && !store::can_add(&digits) {
        diag::warn(&format!(
            "拒绝新增账号：已达上限 {} 个",
            store::MAX_ACCOUNTS
        ));
        return Err(cap_message(true));
    }

    // 登录专用凭据与"当前账号"隔离：始终落在 _pending.json，登录成功后才归位
    let mut cred = store::load(store::NO_KEY);
    let result = auth::send_sms(&mut cred, phone);
    if !result.ok {
        return Err(result.error);
    }
    Ok(SmsCodeDto {
        phone_label: mask_phone(&Credentials::phone_digits(cred.login_phone())),
        resend_seconds: 60,
    })
}

/// 验码登录（阻塞网络）。成功后把 pending 凭据归位到手机号哈希文件。
/// 二次校验上限：发码与确认之间可能已被别的路径加满账号；超限时不落盘，
/// 本次登录结果作废（与旧版 ConfirmLoginAsync 一致）。
fn login_blocking(code: &str) -> Result<LoginResultDto, String> {
    let mut cred = store::load(store::NO_KEY);
    let result = auth::login(&mut cred, code.trim());
    if !result.ok {
        return Err(result.error);
    }
    if !store::can_add(&Credentials::phone_digits(&cred.phone)) {
        cred.block_persist = true;
        diag::warn(&format!(
            "拒绝保存新账号凭据：已达上限 {} 个",
            store::MAX_ACCOUNTS
        ));
        return Err(cap_message(false));
    }
    let key = store::adopt_phone_key(&mut cred);
    if key.is_empty() {
        return Err("登录成功但手机号无效，未能保存账号".to_string());
    }
    diag::info("短信登录成功：账号凭据已保存");
    Ok(LoginResultDto {
        key,
        label: mask_phone(&Credentials::phone_digits(&cred.phone)),
    })
}

/// 发送短信验证码。阻塞网络放 spawn_blocking，避免卡住主线程。
#[tauri::command]
async fn send_sms_code(phone: String, state: State<'_, HomeState>) -> Result<SmsCodeDto, String> {
    let gate = state.auth_gate.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
        send_sms_blocking(&phone)
    })
    .await
    .map_err(|e| format!("发送验证码任务异常：{e}"))?
}

/// 用短信验证码登录。
#[tauri::command]
async fn login_with_code(code: String, state: State<'_, HomeState>) -> Result<LoginResultDto, String> {
    let gate = state.auth_gate.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = gate.lock().unwrap_or_else(|e| e.into_inner());
        login_blocking(&code)
    })
    .await
    .map_err(|e| format!("登录任务异常：{e}"))?
}

// ── 首页只读快照（步 7）──
//
// 旧版 EnterLoggedInAsync → RefreshSnapshotAsync 的等价物：登录成功后先拉一次
// 「任务进度 + 钱包余额」把首页填上。此前首页数据只来自 start_claim 跑完整轮的结果，
// 于是登录后一直显示"-- 时 -- 分""今日暂无任务数据"。
// 两个请求都是只读的（无副作用），失败重试安全；重试预算沿用设置页配置。

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotDto {
    /// 本机是否有已登录账号
    logged_in: bool,
    /// 脱敏手机号（未登录时为空）
    label: String,
    balance_minutes: f64,
    /// 余额是否真的取到；false 时界面显示占位符，不把"没查到"显示成 0
    balance_known: bool,
    /// 今日任务：已完成次数 / 总次数（各阶段次数合计，与旧版同口径）
    claimed: i32,
    total: i32,
    /// 预估可领时长（分钟）：Σ(阶段总次数 × 阶段奖励分钟)，来自服务端真实下发值
    estimated_minutes: i32,
    status_text: String,
    status_is_error: bool,
}

fn snapshot_failure(logged_in: bool, label: &str, status: String, is_error: bool) -> SnapshotDto {
    SnapshotDto {
        logged_in,
        label: label.to_string(),
        balance_minutes: 0.0,
        balance_known: false,
        claimed: 0,
        total: 0,
        estimated_minutes: 0,
        status_text: status,
        status_is_error: is_error,
    }
}

/// 拉一次首页只读快照（阻塞网络，调用方放后台线程）。
fn snapshot_blocking() -> SnapshotDto {
    let Some(account) = store::list_accounts().into_iter().next() else {
        return snapshot_failure(false, "", "登录后同步今日任务".to_string(), false);
    };
    let label = mask_phone(&account.phone_digits);

    let mut cred = store::load(&account.key);
    if !cred.has_token() && !cred.has_refresh_token() {
        return snapshot_failure(
            true,
            &label,
            "本机无登录凭证，需先短信登录".to_string(),
            true,
        );
    }

    let retry = AppSettings::load().network_retry_count;
    // 重试提示：旧版 OnRetryNoticeAsync 同语义，让用户知道程序在重试而不是卡住
    let mut notice = String::new();
    let mut session = Session::new(&mut cred, move || retry);
    if session.jwt_needs_refresh() {
        let r = session.ensure_jwt(false);
        if !r.ok {
            diag::warn(&format!("首页快照：登录态不可用（{}）", r.error));
            return snapshot_failure(true, &label, format!("登录态不可用：{}", r.error), true);
        }
    }
    let mut game = GameClient::new(&mut session);

    let stages = {
        let mut on_retry = |attempt: i32, wait: std::time::Duration| {
            notice = format!(
                "网络波动：任务进度查询失败，{} 秒后自动重试（第 {} 次）",
                wait.as_secs(),
                attempt
            );
        };
        game.get_task_stages(None, false, Some(&mut on_retry))
    };
    let score = {
        let mut on_retry = |attempt: i32, wait: std::time::Duration| {
            notice = format!(
                "网络波动：余额查询失败，{} 秒后自动重试（第 {} 次）",
                wait.as_secs(),
                attempt
            );
        };
        game.get_wallet_score(Some(&mut on_retry))
    };

    let Some(list) = stages.value.as_ref() else {
        let text = describe_reason(stages.reason, &stages.detail);
        diag::warn(&format!("首页快照：任务进度同步失败（{text}）"));
        return snapshot_failure(true, &label, text, true);
    };
    let mut dto = snapshot_failure(true, &label, String::new(), false);
    dto.claimed = list.iter().map(|s| s.stage_current).sum();
    dto.total = list.iter().map(|s| s.stage_sum).sum();
    // 预估可领时长：Σ(阶段全天奖励分值) ÷ SCORE_PER_MINUTE。分值是服务端在每个
    // 阶段的 "total" 字段里真实下发的（会随运营动态变化），不是前端写死的估算。
    let total_score: i64 = list.iter().map(|s| s.total_score.max(0) as i64).sum();
    dto.estimated_minutes = score_to_minutes(total_score).round_ties_even() as i32;

    match score.value {
        Some(score) => {
            dto.balance_minutes = score_to_minutes(score);
            dto.balance_known = true;
        }
        // 任务进度拿到了就先展示，余额单独失败不推翻整块数据
        None => {
            let text = describe_reason(score.reason, &score.detail);
            diag::warn(&format!("首页快照：余额查询失败（{text}）"));
            dto.status_text = text;
            dto.status_is_error = true;
            return dto;
        }
    }

    dto.status_text = if dto.total == 0 {
        "今日暂无任务".to_string()
    } else if dto.claimed >= dto.total {
        "今日任务已全部完成".to_string()
    } else {
        format!(
            "今日任务待领取，还差 {} 次{}",
            dto.total - dto.claimed,
            if notice.is_empty() {
                String::new()
            } else {
                format!("（{notice}）")
            }
        )
    };
    dto.status_is_error = false;
    dto
}

/// 首页只读快照：登录态 + 可用时长 + 今日任务进度。阻塞网络放 spawn_blocking。
#[tauri::command]
async fn get_snapshot() -> Result<SnapshotDto, String> {
    tauri::async_runtime::spawn_blocking(snapshot_blocking)
        .await
        .map_err(|e| format!("同步首页数据异常：{e}"))
}

// ── 领取 ──

/// 每次领取之间的随机间隔（设置页区间内取值）。
/// 用系统时间纳秒取余代替额外 RNG 依赖。
fn claim_interval(min_sec: i32, max_sec: i32) -> Arc<dyn Fn() -> Duration + Send + Sync> {
    Arc::new(move || {
        let span = (max_sec - min_sec).max(0) as u64;
        let extra = if span == 0 {
            0
        } else {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| u64::from(d.subsec_nanos()) % (span + 1))
                .unwrap_or(0)
        };
        Duration::from_secs(min_sec as u64 + extra)
    })
}

/// 当前领取进度（含逐号实时状态：领取线程每推进一步都会写进实时进度板）。
#[tauri::command]
fn get_claim_progress(state: State<'_, HomeState>) -> ClaimProgressDto {
    progress_dto(&lock_runtime(&state.runtime))
}

/// 触发一轮串行领取。只做接线，真正执行沿用步 2 的 AccountSweep。
/// 返回一句状态文案；前端随后轮询 get_claim_progress。
#[tauri::command]
fn start_claim(state: State<'_, HomeState>) -> Result<String, String> {
    let queue = plan_from_store();
    if queue.is_empty() {
        return Err("本机没有已登录账号，请先登录".to_string());
    }

    let cancel = Arc::new(AtomicBool::new(false));
    // 开跑前先把队列铺成"待领取"，界面立刻就能看到要跑哪几个号
    let board: SharedBoard = Arc::new(Mutex::new(LiveBoard {
        entries: queue
            .iter()
            .map(|a| LiveEntry {
                key: a.key.clone(),
                label: a.label(),
                state: "pending",
                claimed: 0,
                total: 0,
                added_minutes: 0,
                balance_minutes: 0.0,
                message: None,
            })
            .collect(),
        note: String::new(),
        log: Vec::new(),
    }));
    {
        let mut runtime = lock_runtime(&state.runtime);
        if runtime.running {
            return Err("正在领取，请等待本轮结束".to_string());
        }
        runtime.running = true;
        runtime.error = false;
        runtime.status = "正在领取…".to_string();
        runtime.last = None;
        runtime.live = board.clone();
        push_live_log(&mut runtime.live.lock().unwrap_or_else(|e| e.into_inner()), "开始本轮领取");
    }

    let settings = AppSettings::load();
    let retry = settings.network_retry_count;
    let (min_sec, max_sec) = settings.interval_secs();
    let interval = claim_interval(min_sec, max_sec);

    let runtime = state.runtime.clone();
    let gate = state.gate.clone();
    std::thread::spawn(move || {
        // 跨进程互斥：计划任务到点拉起的第二个实例可能正在领取同一个账号
        let _cross_process = match ClaimLock::acquire() {
            Some(lock) => Some(lock),
            None => {
                let mut runtime = lock_runtime(&runtime);
                runtime.running = false;
                runtime.error = true;
                runtime.status = "另一个实例正在领取，请稍后重试".to_string();
                return;
            }
        };

        // 逐号开始/结束 → 实时进度板的行状态
        let board_for_sweep = board.clone();
        let sweep_reporter: SweepReporter = Arc::new(move |ev: &SweepEvent| {
            let mut board = lock_board(&board_for_sweep);
            match ev {
                SweepEvent::Started { index, label, .. } => {
                    let total = board.entries.len();
                    if let Some(slot) = board.entries.get_mut(*index) {
                        slot.state = "running";
                        slot.message = None;
                    }
                    push_live_log(
                        &mut board,
                        format!("账号 {label}（{}/{}）开始领取", index + 1, total),
                    );
                }
                SweepEvent::Finished { index, entry } => {
                    if let Some(slot) = board.entries.get_mut(*index) {
                        slot.state = state_name(entry.state);
                        slot.claimed = entry.claimed;
                        slot.total = entry.total;
                        slot.added_minutes = entry.added_minutes;
                        slot.balance_minutes = entry.balance_minutes;
                        slot.message = entry.message.clone();
                    }
                    push_live_log(
                        &mut board,
                        format!(
                            "账号 {} {}：{}/{} 次，+{} 分钟",
                            entry.label,
                            sweep_state_cn(entry.state),
                            entry.claimed,
                            entry.total,
                            entry.added_minutes
                        ),
                    );
                }
            }
        });

        // 逐阶段/逐次领取事件 → 当前号的次数与状态栏文案
        let board_for_claim = board.clone();
        let claim_reporter: Reporter = Arc::new(move |ev: &ClaimEvent| {
            let mut board = lock_board(&board_for_claim);
            board.note = ev.message.clone();
            push_live_log(&mut board, &ev.message);
            // 串行执行，同一时刻只有一个号处于 running
            if let Some(slot) = board.entries.iter_mut().find(|e| e.state == "running") {
                slot.claimed = ev.claimed;
                if ev.total > 0 {
                    slot.total = ev.total;
                }
                if let Some(add) = ev.added_minutes {
                    slot.added_minutes += add;
                }
            }
        });

        let claimant = LiveClaimant::new(retry, interval)
            .with_gate(gate)
            .with_reporter(claim_reporter);
        let mut sweep = AccountSweep::new(queue, claimant)
            .with_cancel_flag(cancel)
            .with_reporter(sweep_reporter);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sweep.run()));

        let mut runtime = lock_runtime(&runtime);
        runtime.running = false;
        match outcome {
            Ok(outcome) => {
                // 收尾落一条"最近领取历史"（未取消且拿到时长/全部完成才写，纯观测不影响领取）
                history::record_from_outcome(&outcome);
                runtime.status = format!(
                    "本轮完成：执行 {}，跳过 {}，失败 {}，+{} 分钟{}",
                    outcome.ran,
                    outcome.skipped,
                    outcome.failed,
                    outcome.total_added_minutes,
                    if outcome.cancelled { "（已取消）" } else { "" }
                );
                runtime.error = outcome.failed > 0;
                runtime.last = Some(outcome);
            }
            Err(_) => {
                runtime.error = true;
                runtime.status = "领取线程异常退出，请查看日志".to_string();
                runtime.last = None;
            }
        }
    });

    Ok("已开始领取".to_string())
}

// ── 定时领取（Windows 计划任务）──
//
// 与旧版一致：不是应用内定时器，而是注册一条每日触发的系统计划任务，
// 到点以 `--scheduled` 拉起本应用完成领取。注册/注销都走 schedule.rs 的真实 schtasks 调用。

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScheduleStatusDto {
    /// 系统里任务处于启用状态（任务不存在或触发器被禁用时为 false）
    enabled: bool,
    /// 任务真实触发时刻 "HH:MM"；任务不存在时回落请求值
    time: String,
    /// 任务是否存在
    exists: bool,
    /// 本次动作是否成功
    succeeded: bool,
    /// 面向用户的说明文案
    message: String,
}

/// 开启/关闭定时领取：开启时注册计划任务（/F 幂等覆盖），关闭时删除任务。
/// 完成后一律回读系统真实状态，并把它写回 schedule.json —— 界面与磁盘都反映实况，
/// 不会出现"开关显示已开、系统里没有任务"的假象。
#[tauri::command]
async fn apply_schedule(enabled: bool, time: String) -> Result<ScheduleStatusDto, String> {
    let mut requested = ScheduleConfig {
        enabled,
        time,
    };
    requested.normalize();
    let exe = std::env::current_exe()
        .map_err(|e| format!("无法确定应用可执行文件路径：{e}"))?;

    let action = tauri::async_runtime::spawn_blocking(move || {
        let result = if requested.enabled {
            schedule::register(&exe, requested.start_time())
        } else {
            schedule::unregister()
        };
        // 无论成败都回读一次真实状态（schtasks 调用最长 15s，必须离开主线程）
        let snapshot = schedule::query_snapshot();
        (result, snapshot, requested)
    })
    .await
    .map_err(|e| format!("定时任务操作异常：{e}"))?;
    let (result, snapshot, requested) = action;

    let (exists, actual_enabled, actual_time) = match snapshot {
        Ok(Some(snapshot)) => (
            true,
            snapshot.enabled && snapshot.trigger_enabled,
            snapshot.start_time.as_hhmm(),
        ),
        Ok(None) => (false, false, requested.time.clone()),
        Err(e) => {
            diag::warn(&format!("计划任务状态无法判定：{e}"));
            (false, false, requested.time.clone())
        }
    };

    // 落盘成实况：开关按任务真实状态，时刻优先取任务时间
    let mut persisted = ScheduleConfig {
        enabled: actual_enabled,
        time: actual_time.clone(),
    };
    persisted.normalize();
    persisted.save();

    let message = match (&result, requested.enabled, exists) {
        (Ok(()), true, _) => format!("已开启定时领取：每天 {actual_time} 自动领取"),
        (Ok(()), false, _) => "已关闭定时领取，系统计划任务已删除".to_string(),
        (Err(e), true, _) => format!("定时任务注册失败：{e}"),
        (Err(e), false, _) => format!("定时任务注销失败：{e}"),
    };

    Ok(ScheduleStatusDto {
        enabled: actual_enabled,
        time: actual_time,
        exists,
        succeeded: result.is_ok(),
        message,
    })
}

// ── 日志读取（步 5）──
//
// 读取当前日志文件尾部并按 diag::write 的行格式切分。行格式固定为
// "yyyy-MM-dd HH:mm:ss.fff [LEVEL] message"；不匹配的行（异常堆栈续行）
// 归并进上一条目，保持界面里一条日志一个色块。

/// 单次最多返回多少行；超过的只取最后 limit 行。
const LOG_DEFAULT_LIMIT: usize = 500;
const LOG_MAX_LIMIT: usize = 5000;
/// 日志文件超过这个体积时只读尾部，避免一次性把大文件读进内存。
const LOG_TAIL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogEntryDto {
    /// 行首时间戳；续行与无法解析的行留空
    time: String,
    /// INFO | WARN | ERROR | 其他等级原样保留
    level: String,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LogPageDto {
    file_name: String,
    exists: bool,
    /// 扫描范围内解析出的日志条数
    total_lines: usize,
    /// 文件过大、只读了尾部时为 true
    truncated: bool,
    entries: Vec<LogEntryDto>,
}

/// 切一行日志。固定前缀 23 字节时间戳 + 空格 + [LEVEL] + 空格。
fn parse_log_line(line: &str) -> Option<(&str, &str, &str)> {
    if line.len() < 27 || !line.is_char_boundary(23) || !line.is_char_boundary(24) {
        return None;
    }
    let bytes = line.as_bytes();
    if bytes[23] != b' ' || bytes[24] != b'[' {
        return None;
    }
    let head = &line[..23];
    // 时间戳形状：yyyy-MM-dd HH:mm:ss.fff
    let shape_ok = head.as_bytes()[4] == b'-'
        && head.as_bytes()[7] == b'-'
        && head.as_bytes()[10] == b' '
        && head.as_bytes()[13] == b':'
        && head.as_bytes()[16] == b':'
        && head.as_bytes()[19] == b'.';
    if !shape_ok {
        return None;
    }
    let rest = &line[25..];
    let close = rest.find(']')?;
    let level = &rest[..close];
    if level.is_empty() || level.len() > 8 {
        return None;
    }
    let message = rest[close + 1..].strip_prefix(' ').unwrap_or("");
    Some((head, level, message))
}

fn read_log_tail(path: &std::path::Path) -> (String, bool) {
    use std::io::{Read, Seek, SeekFrom};

    let Ok(mut file) = std::fs::File::open(path) else {
        return (String::new(), false);
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut truncated = false;
    let mut buf = Vec::new();
    if size > LOG_TAIL_BYTES {
        truncated = true;
        let _ = file.seek(SeekFrom::Start(size - LOG_TAIL_BYTES));
    }
    if file.read_to_end(&mut buf).is_err() {
        return (String::new(), truncated);
    }
    let mut text = String::from_utf8_lossy(&buf).to_string();
    if truncated {
        // 丢掉被截断的首行（缺少行首时间戳的半条记录）
        if let Some(pos) = text.find('\n') {
            text.drain(..=pos);
        }
    }
    (text, truncated)
}

/// 读取当前日志文件尾部。文件不存在时返回 exists=false（首次运行即为该状态）。
/// limit 只是本次返回上限，不改变文件本身。
#[tauri::command]
fn read_logs(limit: Option<usize>) -> LogPageDto {
    let limit = limit.unwrap_or(LOG_DEFAULT_LIMIT).clamp(1, LOG_MAX_LIMIT);
    let path = diag::current_file_path();
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if !path.exists() {
        return LogPageDto {
            file_name,
            exists: false,
            total_lines: 0,
            truncated: false,
            entries: Vec::new(),
        };
    }

    let (text, truncated) = read_log_tail(&path);
    let mut entries: Vec<LogEntryDto> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        match parse_log_line(line) {
            Some((time, level, message)) => entries.push(LogEntryDto {
                time: time.to_string(),
                level: level.to_string(),
                message: message.to_string(),
            }),
            None => {
                // 续行（堆栈、中文异常说明等）并入上一条，保持一色块
                match entries.last_mut() {
                    Some(last) => {
                        last.message.push('\n');
                        last.message.push_str(line);
                    }
                    None => entries.push(LogEntryDto {
                        time: String::new(),
                        level: "DEBUG".to_string(),
                        message: line.to_string(),
                    }),
                }
            }
        }
    }

    let total_lines = entries.len();
    if entries.len() > limit {
        entries.drain(..entries.len() - limit);
    }
    LogPageDto {
        file_name,
        exists: true,
        total_lines,
        truncated,
        entries,
    }
}

/// 最近领取历史（history.json 的最近若干天，按日期升序；前端用本地日期对齐渲染近 7 天）。
#[tauri::command]
fn get_history() -> Vec<glacc::history::HistoryDay> {
    history::last_days(30)
}

// ── 凭据导入 / 导出（步 5）──
//
// 文件选择框用 tauri-plugin-dialog 在 Rust 侧拉起：插件命令不暴露给前端，
// capabilities 无需改动。blocking_* 不允许在主线程调用，因此统一放 spawn_blocking。

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TransferDto {
    cancelled: bool,
    succeeded: bool,
    message: String,
}

fn transfer_cancelled(message: &str) -> TransferDto {
    TransferDto {
        cancelled: true,
        succeeded: false,
        message: message.to_string(),
    }
}

fn export_blocking(app: &tauri::AppHandle) -> Result<TransferDto, String> {
    let stamp: String = diag::local_timestamp()
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    let default_name = format!("glacc-accounts-{stamp}.json");
    let picked = app
        .dialog()
        .file()
        .set_title("导出凭据")
        .add_filter("JSON", &["json"])
        .set_file_name(&default_name)
        .blocking_save_file();
    let Some(path) = picked else {
        return Ok(transfer_cancelled("已取消导出"));
    };
    let path = path
        .into_path()
        .map_err(|e| format!("导出路径无效：{e}"))?;

    match store::export_accounts(&path) {
        Ok(envelope) => {
            let count = envelope.accounts.len();
            diag::info(&format!("已导出 {count} 个账号凭据"));
            Ok(TransferDto {
                cancelled: false,
                succeeded: true,
                message: format!("已导出 {count} 个账号到 {}", path.display()),
            })
        }
        // 导出失败不算异常：把错误文案回给界面展示，不抛 Err
        Err(e) => {
            diag::warn(&format!("导出凭据失败：{e}"));
            Ok(TransferDto {
                cancelled: false,
                succeeded: false,
                message: e.to_string(),
            })
        }
    }
}

fn import_blocking(app: &tauri::AppHandle) -> Result<TransferDto, String> {
    let picked = app
        .dialog()
        .file()
        .set_title("导入凭据")
        .add_filter("JSON", &["json"])
        .blocking_pick_file();
    let Some(path) = picked else {
        return Ok(transfer_cancelled("已取消导入"));
    };
    let path = path
        .into_path()
        .map_err(|e| format!("导入路径无效：{e}"))?;

    match store::import_accounts(&path) {
        Ok(summary) => {
            diag::info(&format!(
                "已导入 {} 个账号凭据，跳过 {} 个",
                summary.imported.len(),
                summary.skipped.len()
            ));
            let mut message = format!("已导入 {} 个账号", summary.imported.len());
            if !summary.skipped.is_empty() {
                // 跳过原因按条列出，最多展示 3 条，避免撑爆界面
                let reasons: Vec<String> = summary
                    .skipped
                    .iter()
                    .take(3)
                    .map(|(phone, why)| format!("{phone}：{why}"))
                    .collect();
                message.push_str(&format!(
                    "，跳过 {} 个（{}）",
                    summary.skipped.len(),
                    reasons.join("；")
                ));
            }
            Ok(TransferDto {
                cancelled: false,
                succeeded: true,
                message,
            })
        }
        Err(e) => {
            diag::warn(&format!("导入凭据失败：{e}"));
            Ok(TransferDto {
                cancelled: false,
                succeeded: false,
                message: e.to_string(),
            })
        }
    }
}

/// 导出全部账号凭据到用户选择的文件。
#[tauri::command]
async fn export_credentials(app: tauri::AppHandle) -> Result<TransferDto, String> {
    tauri::async_runtime::spawn_blocking(move || export_blocking(&app))
        .await
        .map_err(|e| format!("导出任务异常：{e}"))?
}

/// 从用户选择的文件导入账号凭据。
#[tauri::command]
async fn import_credentials(app: tauri::AppHandle) -> Result<TransferDto, String> {
    tauri::async_runtime::spawn_blocking(move || import_blocking(&app))
        .await
        .map_err(|e| format!("导入任务异常：{e}"))?
}

/// 无人值守领取（`--scheduled`）：不建窗口、不依赖 WebView2，跑完一轮后进程退出。
/// 返回进程退出码：0 = 正常结束（含无事可做），1 = 领取阶段异常退出。
///
/// 结果通知走 Server酱：定时运行没有界面可看，通知是唯一可观测面（与旧版一致）。
pub fn run_scheduled() -> i32 {
    diag::prune();
    diag::info("计划任务触发：开始无人值守领取");

    // 跨进程互斥：界面实例可能正在领取，同时打同一出口 IP 违反"串行"红线
    let _lock = match ClaimLock::acquire() {
        Some(lock) => lock,
        None => {
            diag::warn("已有领取任务在运行，本次定时领取跳过");
            return 0;
        }
    };

    let queue = plan_from_store();
    if queue.is_empty() {
        diag::warn("定时领取跳过：本机没有已登录账号");
        return 0;
    }

    let settings = AppSettings::load();
    let (min_sec, max_sec) = settings.interval_secs();
    let claimant =
        LiveClaimant::new(settings.network_retry_count, claim_interval(min_sec, max_sec));
    let mut sweep = AccountSweep::new(queue, claimant);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sweep.run()));

    match outcome {
        Ok(outcome) => {
            history::record_from_outcome(&outcome);
            notify_scheduled_result(&settings.server_key, &outcome);
            0
        }
        Err(_) => {
            diag::error("定时领取异常退出，请查看日志");
            1
        }
    }
}

fn sweep_state_cn(state: SweepState) -> &'static str {
    match state {
        SweepState::Pending => "未执行",
        SweepState::Running => "执行中",
        SweepState::Done => "完成",
        SweepState::Failed => "失败",
        SweepState::Skipped => "跳过",
    }
}

/// 推送定时领取结果（未配置 SendKey 时零请求跳过；失败只记日志）。
fn notify_scheduled_result(send_key: &str, outcome: &SweepOutcome) {
    if !notify::is_configured(send_key) {
        diag::info("未配置 Server酱通知，跳过结果推送");
        return;
    }
    let all_ok = outcome.failed == 0 && outcome.ran > 0;
    let lines: Vec<String> = outcome
        .entries
        .iter()
        .map(|e| match e.state {
            SweepState::Done => format!(
                "✅ {}：{}/{} 次，余额 {} 分钟",
                e.label, e.claimed, e.total, e.balance_minutes.round() as i64
            ),
            SweepState::Skipped => format!(
                "⚠️ {}：跳过（{}）",
                e.label,
                e.message.as_deref().unwrap_or("登录态不可用")
            ),
            other => format!(
                "❌ {}：{}（{}/{} 次）",
                e.label,
                e.message
                    .as_deref()
                    .unwrap_or_else(|| sweep_state_cn(other)),
                e.claimed,
                e.total
            ),
        })
        .collect();
    let head = if all_ok {
        "定时领取完成"
    } else {
        "定时领取未全部完成"
    };
    let desp = format!(
        "**{head}**\n\n- 时间: {}\n- 账号数: {}\n\n{}",
        diag::local_timestamp(),
        outcome.entries.len(),
        lines.join("\n")
    );
    let title = if all_ok {
        "glacc-auto 定时领取成功"
    } else {
        "glacc-auto 定时领取未全部完成"
    };
    notify::send(send_key, title, &desp);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 计划任务到点拉起：同一条命令行走无人值守分支，不建窗口、不载 WebView2。
    if schedule::is_scheduled_launch(std::env::args().skip(1)) {
        std::process::exit(run_scheduled());
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(HomeState::default())
        .setup(|app| {
            // 与旧版 Program.cs 一致：启动时清理 7 天前日志，失败静默
            diag::prune();
            // 开源合规：免责声明写进当天启动日志
            diag::info("免责声明：本项目仅供个人学习与技术交流，严禁商业盈利用途。使用风险由使用者自行承担。");
            // 计划任务对齐放后台：schtasks 最长 15 秒，不能拖慢启动
            std::thread::spawn(|| match std::env::current_exe() {
                Ok(exe) => {
                    if let Err(e) = schedule::reconcile_at_startup(&exe) {
                        diag::warn(&format!("计划任务对齐失败：{e}"));
                    }
                }
                Err(e) => diag::warn(&format!("无法确定应用路径，跳过计划任务对齐：{e}")),
            });
            // 兜底显示：窗口以 visible:false 创建，正常由前端首帧渲染后调 window_show。
            // 前端若加载失败，6 秒后强制显示，避免出现"进程起来了但界面一片空白且看不见"。
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(6));
                if let Some(window) = tauri::Manager::get_webview_window(&handle, "main") {
                    if !window.is_visible().unwrap_or(true) {
                        diag::warn("前端未在 6 秒内请求显示窗口，已兜底显示");
                        let _ = window.show();
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_accounts,
            set_active_account,
            get_snapshot,
            get_claim_progress,
            start_claim,
            logout_account,
            get_settings,
            save_settings,
            apply_schedule,
            send_sms_code,
            login_with_code,
            open_external,
            window_drag,
            window_minimize,
            window_toggle_maximize,
            window_close,
            window_show,
            read_logs,
            get_history,
            export_credentials,
            import_credentials
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
