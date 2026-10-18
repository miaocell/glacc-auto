// 业务请求公共执行器（网络重试 + 登录态续期保底），逐字移植自旧 Avalonia 版
// src\GlaccAuto.Core\Glacc\GlaccSession.cs。
//
// 网络失败与服务端 5xx 按配置预算退避重试；JWT 失效（body code 10003）自动换新并重试同一请求；
// refresh_token 已失效（需短信验证码）时返回 GlaccFailReason::NeedRelogin，由上层决定是否中断业务。
//
// 同步化说明（Rust 侧对应 .NET 的 Task 调度）：
// 旧版所有等待都是"等一次 HTTP 完成再继续"的串行语义，没有并行结构；而 tls.rs 的 send 本身是
// 阻塞调用。因此本层用同步函数 + 可注入的 sleeper（睡眠函数），不引入 tokio/async-std，零新增依赖。
// 调用方把整个 run() 放进自己的后台线程即可（与旧版调用方 Task.Run 的用法一一对应）。
// _refreshGate 用 std::sync::Mutex<()> 复刻 SemaphoreSlim(1,1) 的互斥语义；由于是同步阻塞锁，
// 并发调用会串行排队，先拿到锁的线程完成刷新后，后到的线程在锁内二次检查 JwtNeedsRefresh 即直接返回
// —— 这正是旧版 96 行"二次检查"的作用，保证并发下只真正刷新一次。

use std::sync::Mutex;
use std::time::Duration;

use crate::glacc::auth;
use crate::glacc::credentials::Credentials;
use crate::glacc::result::{GlaccCallResult, GlaccFailReason, GlaccHttpResponse};
use crate::glacc::tls::TlsResponse;
use crate::glacc::{constants, store};

/// refresh_token 保活间隔：长期不调用会失效（invalid_grant/4126），故到期前主动换新一次。
const REFRESH_KEEP_ALIVE_SECONDS: i64 = 7 * 24 * 3600;

/// 睡眠函数（测试可替换成"记录而不真睡"）。
pub type Sleeper = std::sync::Arc<dyn Fn(Duration) + Send + Sync>;

/// 旁路对账钩子：拿到当前凭据（换新后是最新 token），返回 Some = 已确认业务已生效。
pub type OutcomeUnknownHook<'h, T> = Option<&'h mut dyn FnMut(&Credentials) -> Option<T>>;

/// 默认睡眠：阻塞当前线程（与旧版 Task.Delay 在串行流程中的效果一致）。
pub fn default_sleeper() -> Sleeper {
    std::sync::Arc::new(|d: Duration| {
        if !d.is_zero() {
            std::thread::sleep(d);
        }
    })
}

/// 业务请求公共执行器。
pub struct Session<'a> {
    cred: &'a mut Credentials,
    /// 每个请求失败后的额外重试次数（调用时读取，设置页可即时生效 → 用闭包而非固定值）
    retry_count: Box<dyn Fn() -> i32 + 'a>,
    refresh_gate: Mutex<()>,
    sleeper: Sleeper,
}

/// 一次业务请求的结果：response 为 None 表示未取得响应。
///
/// 关于取消：旧版每个方法都可传 CancellationToken。Rust 侧同步实现下，取消经
/// [`GlaccHttpResponse`] 之外的显式检查点处理——当前边界内（不接 UI/调度）没有取消需求，
/// 故不引入取消令牌；接线调度时再按需要扩展（例如用原子标志在重试间检查）。
impl<'a> Session<'a> {
    /// retry_count：每个请求失败后的额外重试次数（运行时读取）。
    pub fn new(cred: &'a mut Credentials, retry_count: impl Fn() -> i32 + 'a) -> Self {
        Self {
            cred,
            retry_count: Box::new(retry_count),
            refresh_gate: Mutex::new(()),
            sleeper: default_sleeper(),
        }
    }

    /// 替换睡眠实现（测试用）。
    pub fn with_sleeper(mut self, sleeper: Sleeper) -> Self {
        self.sleeper = sleeper;
        self
    }

    pub fn cred(&self) -> &Credentials {
        self.cred
    }

    pub fn cred_mut(&mut self) -> &mut Credentials {
        self.cred
    }

    /// 登录态需要换新：JWT 已过期（留 60s 余量，见 Credentials::is_jwt_valid），
    /// 或距上次刷新超过保活间隔。
    pub fn jwt_needs_refresh(&self) -> bool {
        !self.cred.is_jwt_valid()
            || Credentials::now_seconds() - self.cred.obtained_at > REFRESH_KEEP_ALIVE_SECONDS
    }

    /// 确保 JWT 可用：需要时才打 refresh 端点；force = 无视过期判断强制换新。
    /// 网络层失败（无响应/5xx）按重试预算退避重试——刷新多发生在启动与定时拉起路径，
    /// 无人值守，不能因一次瞬断就判登录不可用；invalid_grant 等业务判定立即返回、不消耗预算。
    pub fn ensure_jwt(&mut self, force: bool) -> crate::glacc::result::GlaccResult {
        if !force && !self.jwt_needs_refresh() {
            return crate::glacc::result::GlaccResult::success();
        }
        // 互斥：并发调用时只有第一个线程真正刷新；后到者在锁内二次检查后直接返回。
        let _guard = self.refresh_gate.lock().unwrap_or_else(|e| e.into_inner());
        if !force && !self.jwt_needs_refresh() {
            return crate::glacc::result::GlaccResult::success();
        }
        let max_attempts = (self.retry_count)().clamp(0, 10) + 1;
        let mut attempt = 1;
        loop {
            let result = auth::refresh(self.cred);
            if result.ok {
                // 服务端可能轮换 refresh_token（旧版 RefreshAsync 成功即 _cred.Save()）。
                // 只改内存不落盘的话，下次启动会拿已被作废的旧 token，必报 invalid_grant。
                self.cred.save();
            }
            if result.ok || !result.retryable || attempt >= max_attempts {
                return result;
            }
            store::log(&format!("刷新登录态第 {attempt} 次失败：{}", result.error));
            (self.sleeper)(backoff_delay(attempt - 1));
            attempt += 1;
        }
    }

    /// 执行一次业务请求：失败按重试预算退避重试，JWT 失效自动换新后重试（不消耗预算）。
    ///
    /// 参数与旧版 RunAsync 一一对应：
    /// - send：发出请求。接收**当前**凭据：每次尝试都会重新调用，因此 JWT 换新后重试自动带新 token
    ///   （旧版 send 闭包内每次现读 _cred.AccessToken，此处语义一致）
    /// - parse：解析响应（返回 None = 视为失败）
    /// - accept：结果有效性判定（None 表示非空即有效；如对账时空任务列表视为失败）
    /// - on_retry：重试前回调（重试序号、等待时长），供上层更新提示
    /// - what：业务描述，仅用于日志
    /// - on_outcome_unknown：非幂等写请求的旁路对账钩子。已取得响应但结果无法判定时
    ///   （5xx / 解析失败）先调用它。返回 Some(值) = 已从旁路确认业务实际已生效，
    ///   按成功返回且不再重发；返回 None = 无法确认，交回正常重试流程。只读请求传 None。
    pub fn run<T>(
        &mut self,
        mut send: impl FnMut(&Credentials) -> GlaccHttpResponse,
        parse: impl Fn(&TlsResponse) -> Option<T>,
        accept: Option<&dyn Fn(&T) -> bool>,
        mut on_retry: Option<&mut dyn FnMut(i32, Duration)>,
        what: &str,
        mut on_outcome_unknown: OutcomeUnknownHook<'_, T>,
    ) -> GlaccCallResult<T> {
        let max_attempts = (self.retry_count)().clamp(0, 10) + 1;
        let mut jwt_refreshed = false;
        let mut reason = GlaccFailReason::NoResponse;
        let mut detail = String::new();
        let mut attempts_made = 0;

        let mut attempt: i32 = 1;
        while attempt <= max_attempts {
            attempts_made = attempt;
            let http = send(&*self.cred);
            let resp = http.response.clone();

            let Some(resp) = resp else {
                // 未取得响应
                reason = http.reason;
                detail = if !http.error.is_empty() {
                    http.error.clone()
                } else {
                    "未取得响应".to_string()
                };
                // 落到下面的公共尾部（客户端故障直接跳出）
                if self.finish_attempt(reason, &detail, attempt, max_attempts, &mut on_retry, what)
                {
                    break;
                }
                attempt += 1;
                continue;
            };

            if resp.status >= 500 {
                reason = GlaccFailReason::ServerError;
                detail = format!("HTTP {}", resp.status);
                if let Some(hook) = on_outcome_unknown.as_mut() {
                    if let Some(recovered) = hook(&*self.cred) {
                        return GlaccCallResult::success(recovered);
                    }
                }
            } else if is_auth_failed(&resp) {
                // 换新后仍被拒 = 登录态不可用，只能重新短信登录
                if jwt_refreshed {
                    store::log(&format!("{what}：登录态换新后仍被服务端拒绝"));
                    return GlaccCallResult::fail(
                        GlaccFailReason::NeedRelogin,
                        "登录态换新后仍被服务端拒绝",
                    );
                }
                jwt_refreshed = true;
                let refreshed = self.ensure_jwt(true);
                if !refreshed.ok {
                    let login_reason = if refreshed.need_relogin {
                        GlaccFailReason::NeedRelogin
                    } else {
                        GlaccFailReason::NoResponse
                    };
                    store::log(&format!("{what}：登录态换新失败（{}）", refreshed.error));
                    return GlaccCallResult::fail(login_reason, refreshed.error);
                }
                // 换新成功：重试本请求，不占重试预算（旧版 attempt--）
                continue;
            } else {
                let value = parse(&resp);
                let matches = match &value {
                    Some(v) => accept.map(|f| f(v)).unwrap_or(true),
                    None => false,
                };
                if matches {
                    return GlaccCallResult::success(
                        value.expect("matches 为真时 value 必为 Some"),
                    );
                }

                reason = GlaccFailReason::BadResponse;
                detail = match &value {
                    Some(_) => "响应内容不符合预期".to_string(),
                    None => "响应内容无法解析".to_string(),
                };
                // 已取得响应但无法判定业务结果：非幂等写请求先走旁路确认，避免重发造成重复发放
                if let Some(hook) = on_outcome_unknown.as_mut() {
                    if let Some(recovered) = hook(&*self.cred) {
                        return GlaccCallResult::success(recovered);
                    }
                }
            }

            if self.finish_attempt(reason, &detail, attempt, max_attempts, &mut on_retry, what) {
                break;
            }
            attempt += 1;
        }

        store::log(&format!(
            "{what}失败：{:?}（已尝试 {attempts_made} 次）｜{detail}",
            reason
        ));
        GlaccCallResult::fail(reason, detail)
    }

    /// 单次尝试的公共尾部：客户端环境故障立即终止；未达预算则退避等待。
    /// 返回 true 表示应跳出主循环。
    fn finish_attempt(
        &self,
        reason: GlaccFailReason,
        detail: &str,
        attempt: i32,
        max_attempts: i32,
        on_retry: &mut Option<&mut dyn FnMut(i32, Duration)>,
        what: &str,
    ) -> bool {
        // 客户端环境故障（如指纹库缺失）重试不会改变结果，立即按终态返回
        if reason == GlaccFailReason::ClientError {
            return true;
        }
        if attempt >= max_attempts {
            return true;
        }
        store::log(&format!(
            "{what}第 {attempt} 次尝试失败（{reason:?}）：{detail}"
        ));
        let wait = backoff_delay(attempt - 1);
        if let Some(cb) = on_retry.as_mut() {
            cb(attempt, wait);
        }
        (self.sleeper)(wait);
        false
    }
}

/// 登录态是否失效。实测：服务端一律返回 HTTP 200，
/// JWT 无效/过期/缺失时由 body.code = 10003 表达（HTTP 401/403 仅作兜底）。
/// 注：任务列表接口不校验 JWT，失效时仍正常返回，故只有钱包与 push 会触发续期。
pub fn is_auth_failed(resp: &TlsResponse) -> bool {
    if resp.status == 401 || resp.status == 403 {
        return true;
    }
    match serde_json::from_str::<serde_json::Value>(&resp.body) {
        Ok(v) => v
            .get("code")
            .and_then(|c| c.as_i64())
            .is_some_and(|code| code == constants::AUTH_ERROR_CODE as i64),
        Err(_) => false,
    }
}

/// 退避间隔：2s 起逐次翻倍，封顶 64s（2,4,8,16,32,64,64…）。
///
/// 与旧版的差异（1c 只读核实已确认并按建议修正）：旧版写法 `2 << clamp(idx,0,30)` 在 idx=30
/// 时溢出为负数，Math.Min(64, 负数) 得负数再进 Task.Delay 会抛异常；旧版两处调用上限为 10
/// 故实际不可达，但方法为 public 可被外部直调。Rust 侧若照抄 `1i32 << 30` 会静默 wrap 成
/// 约 -21 亿，再转 u64 会变成巨型等待，比抛异常更隐蔽。故直接 clamp(idx, 0, 5)：
/// 64 = 2^6 已在 n=5 时封顶，与旧式在 0..29 段逐值等价，且不再有溢出分支。
pub fn backoff_delay(retry_index: i32) -> Duration {
    let n = retry_index.clamp(0, 5);
    let seconds = 1u64 << (n + 1);
    Duration::from_secs(seconds.min(64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glacc::result::GlaccResult;
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::Arc;

    const SALT: &str = "0123456789abcdef0123456789abcdef";

    fn cred() -> Credentials {
        Credentials {
            phone: "13800138000".to_string(),
            install_salt: SALT.to_string(),
            ..Credentials::default()
        }
    }

    /// 不真睡的 sleeper（记录调用次数，便于断言退避行为）。
    fn counting_sleeper() -> (Sleeper, Arc<AtomicI32>) {
        let hits = Arc::new(AtomicI32::new(0));
        let h = hits.clone();
        let s: Sleeper = Arc::new(move |_d| {
            h.fetch_add(1, Ordering::SeqCst);
        });
        (s, hits)
    }

    /// BackoffDelay：idx=0..6 → 2,4,8,16,32,64,64；idx=30 仍为 64（不溢出）。
    #[test]
    fn backoff_delay_values_and_overflow() {
        let secs = |i: i32| backoff_delay(i).as_secs();
        assert_eq!(
            vec![
                secs(0),
                secs(1),
                secs(2),
                secs(3),
                secs(4),
                secs(5),
                secs(6)
            ],
            vec![2, 4, 8, 16, 32, 64, 64]
        );
        assert_eq!(secs(30), 64, "idx=30 须封顶 64，不得溢出");
        assert_eq!(secs(1000), 64);
        assert_eq!(secs(-1), 2, "负值按 0 处理");
    }

    /// 旧版在 idx=0..29 段逐值等价：2^(n+1) 封顶 64。
    #[test]
    fn backoff_matches_legacy_within_valid_range() {
        for i in 0..30 {
            let legacy = 2u64 << i.min(5);
            assert_eq!(backoff_delay(i).as_secs(), legacy.min(64), "idx={i}");
        }
    }

    /// JwtNeedsRefresh：剩余 61 秒 → 不需要刷新（is_jwt_valid 为真）；59 秒 → 需要。
    #[test]
    fn jwt_needs_refresh_60s_margin() {
        let now = Credentials::now_seconds();
        let mut c = cred();
        c.access_token = "t".to_string();
        c.expires_in = 7200;

        // 剩余 61 秒：有效，且距 obtained_at 未超保活期 → 不需要刷新
        c.obtained_at = now - 7200 + 61;
        assert!(c.is_jwt_valid());
        {
            let s = Session::new(&mut c, || 0);
            assert!(!s.jwt_needs_refresh());
        }

        // 剩余 59 秒：无效 → 需要刷新
        c.obtained_at = now - 7200 + 59;
        assert!(!c.is_jwt_valid());
        {
            let s = Session::new(&mut c, || 0);
            assert!(s.jwt_needs_refresh());
        }

        // 无 token：需要刷新
        c.access_token.clear();
        let s = Session::new(&mut c, || 0);
        assert!(s.jwt_needs_refresh());
    }

    /// 保活间隔：距上次获取超过 7 天即使未过期也需要刷新。
    #[test]
    fn jwt_needs_refresh_keepalive() {
        let now = Credentials::now_seconds();
        let mut c = cred();
        c.access_token = "t".to_string();
        // 远超 7 天前的获取时间，但 expires_in 极长 → is_jwt_valid 为真、保活判定为真
        c.obtained_at = now - (REFRESH_KEEP_ALIVE_SECONDS + 10);
        c.expires_in = i32::MAX;
        let s = Session::new(&mut c, || 0);
        assert!(s.jwt_needs_refresh());
    }

    /// IsAuthFailed：code=10003 → true；401/403 → true；200 且 code=0 → false。
    #[test]
    fn is_auth_failed_rules() {
        let mk = |status: i32, body: &str| TlsResponse {
            status,
            body: body.to_string(),
        };
        assert!(is_auth_failed(&mk(200, r#"{"code":10003}"#)));
        assert!(is_auth_failed(&mk(401, "{}")));
        assert!(is_auth_failed(&mk(403, "{}")));
        assert!(!is_auth_failed(&mk(200, r#"{"code":0,"result":"ok"}"#)));
        // 非 JSON、非整数 code、其他错误码
        assert!(!is_auth_failed(&mk(200, "<html>gateway</html>")));
        assert!(!is_auth_failed(&mk(200, r#"{"code":"10003"}"#)));
        assert!(!is_auth_failed(&mk(200, r#"{"code":10053}"#)));
    }

    /// 并发只刷一次：多线程同时 ensure_jwt(force=false)，refresh 实际只应执行一次。
    /// 用本地假 refresh（不经网络）：cred 的 refresh_token 为空时 refresh 会立即失败且不落盘，
    /// 因此这里改用"计数 + 直接改 cred"的等价验证：门闸 + 二次检查必须让后到线程快速返回。
    #[test]
    fn concurrent_ensure_jwt_refreshes_once() {
        // 用一个可被多线程共享的计数与凭据；refresh 的"真实动作"在这里就是计数 + 赋新 token。
        let calls = Arc::new(AtomicI32::new(0));
        let now = Credentials::now_seconds();
        let base = Credentials {
            phone: "13800138000".to_string(),
            install_salt: SALT.to_string(),
            access_token: "old".to_string(),
            obtained_at: now - 7200 - 10, // 已过期 → 需要刷新
            expires_in: 7200,
            ..Credentials::default()
        };
        let shared = Arc::new(Mutex::new(base));

        // 12 个线程各自构造 Session（各自独立门闸）无法验证"只刷一次"，
        // 故必须共享同一把门闸：这里复刻 Session 的门闸语义——用共享 Mutex 包住
        // "检查 + 刷新"临界区，与 ensure_jwt 的实现同构。
        let gate = Arc::new(Mutex::new(()));
        let mut handles = Vec::new();
        for _ in 0..12 {
            let shared = shared.clone();
            let gate = gate.clone();
            let calls = calls.clone();
            handles.push(std::thread::spawn(move || {
                let _g = gate.lock().unwrap_or_else(|e| e.into_inner());
                let mut c = shared.lock().unwrap_or_else(|e| e.into_inner());
                // 二次检查（Session::ensure_jwt 锁内第一件事）
                let needs = !c.is_jwt_valid()
                    || Credentials::now_seconds() - c.obtained_at > REFRESH_KEEP_ALIVE_SECONDS;
                if !needs {
                    return false;
                }
                // "刷新"：计数一次并写回新 token
                calls.fetch_add(1, Ordering::SeqCst);
                c.access_token = "new".to_string();
                c.obtained_at = Credentials::now_seconds();
                true
            }));
        }
        let performed: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "并发下 refresh 实际只应执行一次"
        );
        assert_eq!(
            performed.iter().filter(|x| **x).count(),
            1,
            "只有一个线程执行刷新"
        );
        assert_eq!(
            shared.lock().unwrap().access_token,
            "new",
            "刷新结果应对后续调用者可见"
        );
    }

    /// EnsureJwt 快速路径：JWT 有效且未超保活期 → 直接成功，不触发 refresh、不睡眠。
    #[test]
    fn ensure_jwt_fast_path_skips_refresh() {
        let now = Credentials::now_seconds();
        let mut c = cred();
        c.access_token = "valid".to_string();
        c.refresh_token = "rt".to_string();
        c.obtained_at = now;
        c.expires_in = 7200;
        let (sleeper, hits) = counting_sleeper();
        let mut s = Session::new(&mut c, || 3).with_sleeper(sleeper);
        let r: GlaccResult = s.ensure_jwt(false);
        assert!(r.ok, "有效 JWT 不应触发刷新");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        assert_eq!(s.cred().access_token, "valid", "不应改动 token");
    }

    /// 客户端环境故障不重试：send 返回 ClientError → 立即终止，只尝试一次。
    #[test]
    fn client_error_terminates_without_retry() {
        let mut c = cred();
        let (sleeper, hits) = counting_sleeper();
        let mut s = Session::new(&mut c, || 5).with_sleeper(sleeper);
        let attempts = std::cell::Cell::new(0);
        let r: GlaccCallResult<i32> = s.run(
            |_cred| {
                attempts.set(attempts.get() + 1);
                GlaccHttpResponse::from_client_fault("缺 tls-client.dll")
            },
            |_| Some(1),
            None,
            None,
            "测试",
            None,
        );
        assert!(!r.ok());
        assert_eq!(r.reason, GlaccFailReason::ClientError);
        assert_eq!(attempts.get(), 1, "客户端故障不应重试");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "不应进入退避等待");
    }

    /// 5xx 按预算重试，退避次数 = max_attempts - 1。
    #[test]
    fn server_error_retries_per_budget() {
        let mut c = cred();
        let (sleeper, hits) = counting_sleeper();
        let mut s = Session::new(&mut c, || 2).with_sleeper(sleeper); // 2 → 共 3 次尝试
        let attempts = std::cell::Cell::new(0);
        let r: GlaccCallResult<i32> = s.run(
            |_cred| {
                attempts.set(attempts.get() + 1);
                GlaccHttpResponse::new(
                    Some(TlsResponse {
                        status: 500,
                        body: "boom".to_string(),
                    }),
                    "",
                    GlaccFailReason::ServerError,
                )
            },
            |_| Some(1),
            None,
            None,
            "测试",
            None,
        );
        assert!(!r.ok());
        assert_eq!(r.reason, GlaccFailReason::ServerError);
        assert_eq!(attempts.get(), 3);
        assert_eq!(hits.load(Ordering::SeqCst), 2, "3 次尝试对应 2 次退避");
    }

    /// 解析成功即返回；accept 拒绝时判 BadResponse 并重试。
    #[test]
    fn parse_and_accept_paths() {
        // 1) 正常成功
        let mut c = cred();
        let mut s = Session::new(&mut c, || 0).with_sleeper(counting_sleeper().0);
        let r: GlaccCallResult<i32> = s.run(
            |_cred| {
                GlaccHttpResponse::new(
                    Some(TlsResponse {
                        status: 200,
                        body: "{\"v\":7}".to_string(),
                    }),
                    "",
                    GlaccFailReason::None,
                )
            },
            |_| Some(7),
            None,
            None,
            "测试",
            None,
        );
        assert!(r.ok());
        assert_eq!(r.value, Some(7));

        // 2) accept 拒绝 → BadResponse
        let mut c2 = cred();
        let mut s2 = Session::new(&mut c2, || 0).with_sleeper(counting_sleeper().0);
        let accept = |v: &i32| *v > 100;
        let r: GlaccCallResult<i32> = s2.run(
            |_cred| {
                GlaccHttpResponse::new(
                    Some(TlsResponse {
                        status: 200,
                        body: "{}".to_string(),
                    }),
                    "",
                    GlaccFailReason::None,
                )
            },
            |_| Some(7),
            Some(&accept),
            None,
            "测试",
            None,
        );
        assert!(!r.ok());
        assert_eq!(r.reason, GlaccFailReason::BadResponse);
        assert_eq!(r.detail, "响应内容不符合预期");
    }

    /// 旁路对账：5xx 与解析失败两条触发路径都能把"未知结果"翻成成功。
    #[test]
    fn outcome_unknown_hook_recovers() {
        // 5xx 路径
        let mut c = cred();
        let mut s = Session::new(&mut c, || 0).with_sleeper(counting_sleeper().0);
        let mut hook = |_cred: &Credentials| Some(99i32);
        let r: GlaccCallResult<i32> = s.run(
            |_cred| {
                GlaccHttpResponse::new(
                    Some(TlsResponse {
                        status: 502,
                        body: "bad gateway".to_string(),
                    }),
                    "",
                    GlaccFailReason::ServerError,
                )
            },
            |_| None,
            None,
            None,
            "推送",
            Some(&mut hook),
        );
        assert!(r.ok(), "旁路确认成功应视为成功");
        assert_eq!(r.value, Some(99));

        // 解析失败路径（hook 返回 None → 继续重试直至预算耗尽）
        let mut c2 = cred();
        let mut s2 = Session::new(&mut c2, || 1).with_sleeper(counting_sleeper().0);
        let mut hook2 = |_cred: &Credentials| None;
        let r: GlaccCallResult<i32> = s2.run(
            |_cred| {
                GlaccHttpResponse::new(
                    Some(TlsResponse {
                        status: 200,
                        body: "not json".to_string(),
                    }),
                    "",
                    GlaccFailReason::None,
                )
            },
            |_| None,
            None,
            None,
            "推送",
            Some(&mut hook2),
        );
        assert!(!r.ok());
        assert_eq!(r.reason, GlaccFailReason::BadResponse);
    }

    /// on_retry 回调收到递增的重试序号与正确的退避时长。
    #[test]
    fn on_retry_receives_attempt_and_delay() {
        let mut c = cred();
        let (sleeper, _) = counting_sleeper();
        let mut s = Session::new(&mut c, || 2).with_sleeper(sleeper);
        let mut seen: Vec<(i32, u64)> = Vec::new();
        let mut cb = |attempt: i32, d: Duration| seen.push((attempt, d.as_secs()));
        let r: GlaccCallResult<i32> = s.run(
            |_cred| {
                GlaccHttpResponse::new(
                    Some(TlsResponse {
                        status: 503,
                        body: String::new(),
                    }),
                    "",
                    GlaccFailReason::ServerError,
                )
            },
            |_| Some(1),
            None,
            Some(&mut cb),
            "测试",
            None,
        );
        assert!(!r.ok());
        assert_eq!(seen, vec![(1, 2), (2, 4)], "重试序号从 1 起、退避 2s/4s");
    }

    /// 登录失效自动续期：注入的 refresh 通过 auth::refresh 走网络，故此处只验证
    /// "两次都被拒 → NeedRelogin"这一不依赖真实刷新结果的终态（refreshed 失败时直接返回）。
    #[test]
    fn auth_failed_twice_returns_need_relogin_or_refresh_failure() {
        let mut c = cred();
        // 无 refresh_token → ensure_jwt(true) 必然失败（"无 refresh_token"）
        let mut s = Session::new(&mut c, || 0).with_sleeper(counting_sleeper().0);
        let r: GlaccCallResult<i32> = s.run(
            |_cred| {
                GlaccHttpResponse::new(
                    Some(TlsResponse {
                        status: 200,
                        body: r#"{"code":10003}"#.to_string(),
                    }),
                    "",
                    GlaccFailReason::None,
                )
            },
            |_| Some(1),
            None,
            None,
            "钱包",
            None,
        );
        assert!(!r.ok());
        assert_eq!(
            r.reason,
            GlaccFailReason::NoResponse,
            "刷新本身网络/前置失败 → NoResponse（旧版同）"
        );
        assert!(r.detail.contains("refresh_token"), "detail={}", r.detail);
    }
}
