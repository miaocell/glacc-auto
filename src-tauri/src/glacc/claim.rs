// 单账号领取执行器，逐字移植自旧 Avalonia 版
// src\GlaccAuto.Core\Claiming\ClaimRunner.cs。
//
// 流程：拉取服务端当日全部主任务 → 按阶段逐次推送 → 每阶段结束后与该主任务对账 → 刷新余额。
// 任务配置（masterTaskId / taskId / stageSum）一律现取，不硬编码。
//
// Rust 侧与旧版的映射：
// - async/await → 同步阻塞 + 可注入 Waiter（不引入 tokio；与 session/tls 的同步模型一致）。
// - CancellationToken → Arc<AtomicBool> + 可被取消唤醒的等待器；取消在等待点即时生效。
// - 并发：旧版靠调用方串行（AccountSweep）；这里再加一道进程内串行门闸，重复调用直接拒绝，
//   绝不排队阻塞界面线程，更不会让两个账号同时打同一个出口 IP。
// - 旧版取消时抛 OperationCanceledException；此处返回 cancelled = true 的结果，进度保留。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use crate::glacc::credentials::Credentials;
use crate::glacc::game::{GameApi, PushResult, TaskStage};
use crate::glacc::result::{GlaccCallResult, GlaccFailReason};
use crate::glacc::store;
use crate::glacc::constants;

/// 阶段结束后的对账等待：实测服务端 ≤2s 落账，留 3s 余量。
pub const SETTLE_DELAY: Duration = Duration::from_secs(3);

/// 单个任务阶段（taskId 粒度）的领取结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageOutcome {
    pub master_task_id: i32,
    pub task_id: i32,
    pub name: String,
    pub claimed: i32,
    pub sum: i32,
    pub added_minutes: i32,
    pub refused_code: Option<i32>,
}

/// 一个账号本轮的领取结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ClaimOutcome {
    pub account_label: String,
    pub completed: bool,
    pub already_done: bool,
    pub claimed: i32,
    pub total: i32,
    pub added_minutes: i32,
    pub balance_minutes: f64,
    pub stages: Vec<StageOutcome>,
    pub failure: Option<String>,
    /// 失败原因枚举（Rust 侧新增）：串行编排据此把"需重新登录/尚未登录"判为跳过而非失败。
    pub fail_reason: Option<GlaccFailReason>,
    /// 本次运行是否因取消而提前结束（Rust 侧新增：旧版以异常表达，调度层需要区分"取消"与"失败"）。
    pub cancelled: bool,
}

/// 领取过程中的进度事件（供界面与日志同步展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimEvent {
    pub message: String,
    pub claimed: i32,
    pub total: i32,
    pub added_minutes: Option<i32>,
}

/// 等待器：生产实现可被取消立刻唤醒；测试注入"记录而不真睡"。
pub trait Waiter: Send + Sync {
    /// 等待指定时长；返回 false 表示被取消。
    fn wait(&self, d: Duration) -> bool;
}

/// 默认可取消等待器：分片睡眠并轮询取消标志，避免最长 3600s 的间隔等待无法中断。
pub struct CancellableWaiter {
    cancel: Arc<AtomicBool>,
}

impl CancellableWaiter {
    pub fn new(cancel: Arc<AtomicBool>) -> Self {
        Self { cancel }
    }
}

impl Waiter for CancellableWaiter {
    fn wait(&self, d: Duration) -> bool {
        let start = Instant::now();
        loop {
            if self.cancel.load(Ordering::SeqCst) {
                return false;
            }
            let elapsed = start.elapsed();
            if elapsed >= d {
                return true;
            }
            std::thread::sleep((d - elapsed).min(Duration::from_millis(50)));
        }
    }
}

/// 进度回调（旧版 Action<ClaimEvent>）。
pub type Reporter = Arc<dyn Fn(&ClaimEvent) + Send + Sync>;

fn noop_reporter() -> Reporter {
    Arc::new(|_| {})
}

/// 进程内串行门闸：同一时刻只允许一个领取任务运行。
pub type SerialGate = Arc<Mutex<()>>;

pub fn new_serial_gate() -> SerialGate {
    Arc::new(Mutex::new(()))
}

/// 领取执行器。持有 game 的所有权（game 可借 Session），便于调度层把整轮领取放进后台线程。
pub struct ClaimRunner<G: GameApi> {
    phone: String,
    game: G,
    /// 每次领取之间的随机间隔（设置页决定，调用时读取）
    interval: Arc<dyn Fn() -> Duration + Send + Sync>,
    waiter: Arc<dyn Waiter>,
    cancel: Arc<AtomicBool>,
    gate: SerialGate,
    reporter: Reporter,
}

impl<G: GameApi> ClaimRunner<G> {
    pub fn new(
        phone: impl Into<String>,
        game: G,
        interval: Arc<dyn Fn() -> Duration + Send + Sync>,
    ) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let waiter: Arc<dyn Waiter> = Arc::new(CancellableWaiter::new(cancel.clone()));
        Self {
            phone: phone.into(),
            game,
            interval,
            waiter,
            cancel,
            gate: new_serial_gate(),
            reporter: noop_reporter(),
        }
    }

    /// 注入等待器（测试用；仍受 cancel 标志检查约束）。
    pub fn with_waiter(mut self, waiter: Arc<dyn Waiter>) -> Self {
        self.waiter = waiter;
        self
    }

    /// 复用外部取消标志（sweep 的全局取消直接传给每个 ClaimRunner）：
    /// 标志置位后本轮领取在下个等待/检查点退出。
    pub fn with_cancel_flag(mut self, cancel: Arc<AtomicBool>) -> Self {
        // 等待器也要换成跟随新标志的实现，否则旧等待器仍盯着旧的（永不置位的）标志。
        self.waiter = Arc::new(CancellableWaiter::new(cancel.clone()));
        self.cancel = cancel;
        self
    }

    /// 共用串行门闸（调度层让全部账号共享同一实例）。
    pub fn with_gate(mut self, gate: SerialGate) -> Self {
        self.gate = gate;
        self
    }

    /// 注入进度回调（界面/日志）。
    pub fn with_reporter(mut self, reporter: Reporter) -> Self {
        self.reporter = reporter;
        self
    }

    /// 取消标志句柄：调度层可克隆后跨线程请求取消。
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// 请求取消（等价旧版 CancellationTokenSource.Cancel；在下一个等待/检查点生效）。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    fn report(&self, message: String, claimed: i32, total: i32, added_minutes: Option<i32>) {
        (self.reporter)(&ClaimEvent {
            message,
            claimed,
            total,
            added_minutes,
        });
    }

    /// 执行一轮领取。取消时立即停止，已完成部分保留在服务端。
    pub fn run(&mut self) -> ClaimOutcome {
        let label = mask_phone(&Credentials::phone_digits(&self.phone));

        // 串行门闸：非阻塞获取；已被占用直接拒绝（绝不排队，避免界面线程等待数十分钟）。
        let gate = self.gate.clone();
        let _guard = match gate.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => {
                return ClaimOutcome {
                    account_label: label,
                    completed: false,
                    already_done: false,
                    claimed: 0,
                    total: 0,
                    added_minutes: 0,
                    balance_minutes: 0.0,
                    stages: Vec::new(),
                    failure: Some("已有领取任务在运行（严格串行，拒绝并发）".to_string()),
                    fail_reason: None,
                    cancelled: false,
                }
            }
        };

        self.run_locked(&label)
    }

    fn run_locked(&mut self, label: &str) -> ClaimOutcome {
        if self.cancelled() {
            return cancelled_outcome(label, 0, 0, 0, Vec::new());
        }

        let stages_result = {
            let reporter = self.reporter.clone();
            let label_owned = label.to_string();
            let mut on_retry = move |attempt: i32, wait: Duration| {
                report_retry(&reporter, &label_owned, "任务进度查询", attempt, wait)
            };
            self.game
                .get_task_stages(None, true, Some(&mut on_retry))
        };

        if !stages_result.ok() {
            return fail(
                label,
                stages_result.reason,
                &stages_result.detail,
                Vec::new(),
            );
        }

        let stages = stages_result.value.unwrap_or_default();
        let total: i32 = stages.iter().map(|s| s.stage_sum).sum();
        let done_at_start: i32 = stages.iter().map(|s| s.stage_current).sum();

        if total == 0 || done_at_start >= total {
            self.report(
                format!("账号 {label}：今日已全部领取"),
                done_at_start,
                total,
                None,
            );
            let balance = self.wallet(label);
            if self.cancelled() {
                return cancelled_outcome(
                    label,
                    done_at_start,
                    total,
                    0,
                    snapshot_stages(&stages),
                );
            }
            return ClaimOutcome {
                account_label: label.to_string(),
                completed: true,
                already_done: true,
                claimed: done_at_start,
                total,
                added_minutes: 0,
                balance_minutes: balance.map(score_to_minutes).unwrap_or(0.0),
                stages: snapshot_stages(&stages),
                failure: None,
                fail_reason: None,
                cancelled: false,
            };
        }

        self.report(
            format!("账号 {label}：开始领取 {} 次", total - done_at_start),
            done_at_start,
            total,
            None,
        );

        // 本地已确认次数按 taskId 记账：多主任务下同名阶段必须分开计。
        // （旧版 ToDictionary 遇到重复 taskId 会抛异常；此处按 taskId 聚合，协议内 taskId 唯一。）
        let mut claimed_by_task: HashMap<i32, i32> = HashMap::new();
        let mut added_by_task: HashMap<i32, i32> = HashMap::new();
        for s in &stages {
            claimed_by_task.entry(s.task_id).or_insert(0);
            added_by_task.entry(s.task_id).or_insert(0);
        }
        let mut refused: HashMap<i32, i32> = HashMap::new();
        let mut claim_index = done_at_start;

        for stage in &stages {
            if self.cancelled() {
                return cancelled_outcome(
                    label,
                    claim_index,
                    total,
                    sum_added(&added_by_task),
                    build_stages(&stages, &claimed_by_task, &added_by_task, &refused),
                );
            }

            while stage.stage_current + claimed_of(&claimed_by_task, stage.task_id)
                < stage.stage_sum
            {
                if !self.waiter.wait((self.interval)()) {
                    return cancelled_outcome(
                        label,
                        claim_index,
                        total,
                        sum_added(&added_by_task),
                        build_stages(&stages, &claimed_by_task, &added_by_task, &refused),
                    );
                }
                if self.cancelled() {
                    return cancelled_outcome(
                        label,
                        claim_index,
                        total,
                        sum_added(&added_by_task),
                        build_stages(&stages, &claimed_by_task, &added_by_task, &refused),
                    );
                }

                let local_current = stage.stage_current + claimed_of(&claimed_by_task, stage.task_id);
                let push = {
                    let reporter = self.reporter.clone();
                    let label_owned = label.to_string();
                    let what = format!("{} 推送", stage.name);
                    let mut on_retry = move |attempt: i32, wait: Duration| {
                        report_retry(&reporter, &label_owned, &what, attempt, wait)
                    };
                    self.game.push_task(
                        stage.master_task_id,
                        stage.task_id,
                        local_current,
                        Some(&mut on_retry),
                    )
                };

                if !push.ok() {
                    // 网络/登录态类失败：整号停止（与旧版一致，已领取部分保留在服务端）
                    return fail(
                        label,
                        push.reason,
                        &push.detail,
                        build_stages(&stages, &claimed_by_task, &added_by_task, &refused),
                    );
                }

                let result = push.value.unwrap_or(PushResult {
                    code: -1,
                    add_score: 0,
                    task_name: String::new(),
                });
                if result.code != 0 {
                    // -1702 = 阶段已满；其他 code = 服务端限制。记下后进入下一阶段
                    refused.insert(stage.task_id, result.code);
                    self.report(
                        format!("{}：服务端拒绝 code={}", stage.name, result.code),
                        claim_index,
                        total,
                        None,
                    );
                    break;
                }

                *claimed_by_task.entry(stage.task_id).or_insert(0) += 1;
                let minutes = score_to_minutes_rounded(result.add_score);
                *added_by_task.entry(stage.task_id).or_insert(0) += minutes;
                claim_index = claim_index_of(&stages, &claimed_by_task);
                self.report(
                    format!("{}：+{minutes} 分钟", stage.name),
                    claim_index,
                    total,
                    Some(minutes),
                );
            }

            // 阶段结束对账：等 3s 让服务端落账（实测 ≤2s），只查该主任务，按 taskId 纠正本地计数
            if !self.waiter.wait(SETTLE_DELAY) {
                return cancelled_outcome(
                    label,
                    claim_index,
                    total,
                    sum_added(&added_by_task),
                    build_stages(&stages, &claimed_by_task, &added_by_task, &refused),
                );
            }
            if self.cancelled() {
                return cancelled_outcome(
                    label,
                    claim_index,
                    total,
                    sum_added(&added_by_task),
                    build_stages(&stages, &claimed_by_task, &added_by_task, &refused),
                );
            }

            let server = {
                let reporter = self.reporter.clone();
                let label_owned = label.to_string();
                let mut on_retry = move |attempt: i32, wait: Duration| {
                    report_retry(&reporter, &label_owned, "任务进度对账", attempt, wait)
                };
                self.game
                    .get_task_stages(Some(stage.master_task_id), true, Some(&mut on_retry))
            };
            if !server.ok() {
                continue; // 对账失败不推翻已确认的领取
            }

            let Some(remote) = server
                .value
                .unwrap_or_default()
                .into_iter()
                .find(|s| s.task_id == stage.task_id)
            else {
                continue;
            };

            let confirmed = claimed_of(&claimed_by_task, stage.task_id);
            if remote.stage_current >= stage.stage_current + confirmed {
                // 服务端更高（他处领取/跳过等）以服务端为准，不再重复推
                claimed_by_task.insert(stage.task_id, remote.stage_current - stage.stage_current);
            } else {
                refused.insert(stage.task_id, -1);
                self.report(
                    format!(
                        "{}：对账异常（本地 {} / 服务端 {}）",
                        stage.name,
                        stage.stage_current + confirmed,
                        remote.stage_current
                    ),
                    claim_index,
                    total,
                    None,
                );
            }
            claim_index = claim_index_of(&stages, &claimed_by_task);
        }

        if self.cancelled() {
            return cancelled_outcome(
                label,
                claim_index,
                total,
                sum_added(&added_by_task),
                build_stages(&stages, &claimed_by_task, &added_by_task, &refused),
            );
        }

        let wallet = self.wallet(label);
        if self.cancelled() {
            return cancelled_outcome(
                label,
                claim_index,
                total,
                sum_added(&added_by_task),
                build_stages(&stages, &claimed_by_task, &added_by_task, &refused),
            );
        }

        let all_done = stages
            .iter()
            .all(|s| s.stage_current + claimed_of(&claimed_by_task, s.task_id) >= s.stage_sum);
        let outcome = ClaimOutcome {
            account_label: label.to_string(),
            completed: all_done && refused.is_empty(),
            already_done: false,
            claimed: claim_index,
            total,
            added_minutes: sum_added(&added_by_task),
            balance_minutes: wallet.map(score_to_minutes).unwrap_or(0.0),
            stages: build_stages(&stages, &claimed_by_task, &added_by_task, &refused),
            failure: None,
            fail_reason: None,
            cancelled: false,
        };

        self.report(
            format!(
                "账号 {label}：{}/{} 次，本次 +{} 分钟",
                outcome.claimed, total, outcome.added_minutes
            ),
            claim_index,
            total,
            None,
        );
        outcome
    }

    fn wallet(&mut self, label: &str) -> Option<i64> {
        let reporter = self.reporter.clone();
        let label_owned = label.to_string();
        let mut on_retry = move |attempt: i32, wait: Duration| {
            report_retry(&reporter, &label_owned, "余额查询", attempt, wait)
        };
        let result: GlaccCallResult<i64> = self.game.get_wallet_score(Some(&mut on_retry));
        if result.ok() {
            result.value
        } else {
            None
        }
    }
}

fn report_retry(reporter: &Reporter, label: &str, what: &str, attempt: i32, wait: Duration) {
    let who = if label.is_empty() {
        String::new()
    } else {
        format!("{label}：")
    };
    reporter(&ClaimEvent {
        message: format!(
            "{who}{what} 第 {attempt} 次失败，{:.0} 秒后自动重试",
            wait.as_secs_f64()
        ),
        claimed: 0,
        total: 0,
        added_minutes: None,
    });
}

fn claimed_of(map: &HashMap<i32, i32>, task_id: i32) -> i32 {
    map.get(&task_id).copied().unwrap_or(0)
}

fn sum_added(map: &HashMap<i32, i32>) -> i32 {
    map.values().sum()
}

fn claim_index_of(stages: &[TaskStage], claimed: &HashMap<i32, i32>) -> i32 {
    stages
        .iter()
        .map(|s| s.stage_current + claimed_of(claimed, s.task_id))
        .sum()
}

/// 手机号脱敏显示（前 2 + 6 星 + 后 3）。
pub fn mask_phone(phone_digits: &str) -> String {
    let chars: Vec<char> = phone_digits.chars().collect();
    if chars.len() <= 5 {
        return phone_digits.to_string();
    }
    let head: String = chars[..2].iter().collect();
    let tail: String = chars[chars.len() - 3..].iter().collect();
    format!("{head}******{tail}")
}

/// 钱包分值 → 分钟（80 分 = 1 分钟，官方换算）。
pub fn score_to_minutes(score: i64) -> f64 {
    score as f64 / constants::SCORE_PER_MINUTE
}

/// 分钟取整：复刻 .NET Math.Round 的"银行家舍入"（四舍六入五取偶），
/// 与 Rust f64::round 的"五入远离零"在 .5 处不同，必须显式用 round_ties_even。
fn score_to_minutes_rounded(score: i64) -> i32 {
    score_to_minutes(score).round_ties_even() as i32
}

fn cancelled_outcome(
    label: &str,
    claimed: i32,
    total: i32,
    added_minutes: i32,
    stages: Vec<StageOutcome>,
) -> ClaimOutcome {
    ClaimOutcome {
        account_label: label.to_string(),
        completed: false,
        already_done: false,
        claimed,
        total,
        added_minutes,
        balance_minutes: 0.0,
        stages,
        failure: None,
        fail_reason: None,
        cancelled: true,
    }
}

fn fail(
    label: &str,
    reason: GlaccFailReason,
    detail: &str,
    stages: Vec<StageOutcome>,
) -> ClaimOutcome {
    let why = describe_reason(reason, detail);
    store::log(&format!("账号 {label} 领取失败：{reason:?}｜{detail}"));
    ClaimOutcome {
        account_label: label.to_string(),
        completed: false,
        already_done: false,
        claimed: 0,
        total: 0,
        added_minutes: 0,
        balance_minutes: 0.0,
        stages,
        failure: Some(why),
        fail_reason: Some(reason),
        cancelled: false,
    }
}

/// 失败原因 → 可读文案（与旧版 DescribeReason 逐条一致）。
pub fn describe_reason(reason: GlaccFailReason, detail: &str) -> String {
    match reason {
        GlaccFailReason::NoResponse => format!("网络不可用：{detail}"),
        GlaccFailReason::ServerError => format!("服务端暂时不可用：{detail}"),
        GlaccFailReason::BadResponse => format!("响应无法解析（接口可能已变更）：{detail}"),
        GlaccFailReason::ClientError => format!("本机 TLS 指纹库不可用：{detail}"),
        GlaccFailReason::NeedRelogin => "登录已过期，需要重新短信登录".to_string(),
        GlaccFailReason::NotLoggedIn => "尚未登录".to_string(),
        GlaccFailReason::None => detail.to_string(),
    }
}

fn build_stages(
    stages: &[TaskStage],
    claimed: &HashMap<i32, i32>,
    added: &HashMap<i32, i32>,
    refused: &HashMap<i32, i32>,
) -> Vec<StageOutcome> {
    stages
        .iter()
        .map(|s| StageOutcome {
            master_task_id: s.master_task_id,
            task_id: s.task_id,
            name: s.name.clone(),
            claimed: s.stage_current + claimed_of(claimed, s.task_id),
            sum: s.stage_sum,
            added_minutes: added.get(&s.task_id).copied().unwrap_or(0),
            refused_code: refused.get(&s.task_id).copied(),
        })
        .collect()
}

fn snapshot_stages(stages: &[TaskStage]) -> Vec<StageOutcome> {
    stages
        .iter()
        .map(|s| StageOutcome {
            master_task_id: s.master_task_id,
            task_id: s.task_id,
            name: s.name.clone(),
            claimed: s.stage_current,
            sum: s.stage_sum,
            added_minutes: 0,
            refused_code: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glacc::game::{GameClient, Transport};
    use crate::glacc::result::GlaccHttpResponse;
    use crate::glacc::session::{Session, Sleeper};
    use crate::glacc::tls::{TlsRequestPayload, TlsResponse};
    use std::collections::VecDeque;
    use std::sync::{Barrier, Condvar};

    const SALT: &str = "0123456789abcdef0123456789abcdef";
    const INTERVAL_SECS: u64 = 5;

    fn cred() -> Credentials {
        Credentials {
            phone: "13800138000".to_string(),
            install_salt: SALT.to_string(),
            sub: "sub-1".to_string(),
            access_token: "token".to_string(),
            ..Credentials::default()
        }
    }

    fn stage(mid: i32, tid: i32, name: &str, cur: i32, sum: i32) -> TaskStage {
        TaskStage {
            master_task_id: mid,
            task_id: tid,
            name: name.to_string(),
            stage_current: cur,
            stage_sum: sum,
            total_score: 0,
            spark_enabled: false,
            spark_reward_minutes: 0,
            spark_claimed: false,
        }
    }

    fn ok_list(stages: Vec<TaskStage>) -> GlaccCallResult<Vec<TaskStage>> {
        GlaccCallResult::success(stages)
    }

    fn ok_push(add_score: i64) -> GlaccCallResult<PushResult> {
        GlaccCallResult::success(PushResult {
            code: 0,
            add_score,
            task_name: String::new(),
        })
    }

    /// 脚本化传输层 + 等待记录（同一把尺子量"注入的假等待时长"）。
    struct Harness {
        script: Mutex<VecDeque<GlaccHttpResponse>>,
        seen: Mutex<Vec<(String, String)>>,
        waits: Arc<Mutex<Vec<u64>>>,
    }

    impl Harness {
        fn new(script: Vec<GlaccHttpResponse>) -> Arc<Self> {
            Arc::new(Self {
                script: Mutex::new(script.into()),
                seen: Mutex::new(Vec::new()),
                waits: Arc::new(Mutex::new(Vec::new())),
            })
        }

        fn transport(self: &Arc<Self>) -> Transport {
            let me = self.clone();
            Arc::new(move |p: &TlsRequestPayload| {
                me.seen
                    .lock()
                    .unwrap()
                    .push((p.request_method.clone(), p.request_url.clone()));
                me.script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| GlaccHttpResponse::no_response("脚本已耗尽"))
            })
        }

        fn sleeper(self: &Arc<Self>) -> Sleeper {
            let waits = self.waits.clone();
            Arc::new(move |d: Duration| waits.lock().unwrap().push(d.as_secs()))
        }

        fn waiter(self: &Arc<Self>) -> Arc<dyn Waiter> {
            struct W(Arc<Mutex<Vec<u64>>>);
            impl Waiter for W {
                fn wait(&self, d: Duration) -> bool {
                    self.0.lock().unwrap().push(d.as_secs());
                    true
                }
            }
            Arc::new(W(self.waits.clone()))
        }

        fn waits(&self) -> Vec<u64> {
            self.waits.lock().unwrap().clone()
        }

        fn methods(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|(m, _)| m.clone())
                .collect()
        }

        fn urls(&self) -> Vec<String> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .map(|(_, u)| u.clone())
                .collect()
        }
    }

    fn json(body: &str) -> GlaccHttpResponse {
        GlaccHttpResponse::new(
            Some(TlsResponse {
                status: 200,
                body: body.to_string(),
            }),
            "",
            GlaccFailReason::None,
        )
    }

    fn status(code: i32) -> GlaccHttpResponse {
        GlaccHttpResponse::new(
            Some(TlsResponse {
                status: code,
                body: String::new(),
            }),
            "",
            if code >= 500 {
                GlaccFailReason::ServerError
            } else {
                GlaccFailReason::None
            },
        )
    }

    /// 固定 JSON（多主任务 + 前缀裁剪 + 火花字段）驱动完整领取：
    /// 断言解析结果、推送次数、循环顺序与注入的假等待时长。
    #[test]
    fn claim_flow_with_fixed_json_matches_legacy() {
        let list = r#"{"code":0,"data":{"list":[
            {"masterTaskId":52,"taskName":"移动端-点广告得时长","taskDetailList":[
                {"taskId":178,"taskName":"阶段一","stageSum":1,"stageCurrent":0,
                 "sparkEnabled":true,"sparkRewardMinutes":5,"sparkClaimed":false},
                {"taskId":179,"taskName":"阶段二","stageSum":1,"stageCurrent":0}
            ]},
            {"masterTaskId":78,"taskName":"移动端-每日任务","taskDetailList":[
                {"taskId":201,"taskName":"浏览","stageSum":1,"stageCurrent":0}
            ]}
        ]}}"#;
        let after_52 = r#"{"code":0,"data":{"list":[
            {"masterTaskId":52,"taskName":"移动端-点广告得时长","taskDetailList":[
                {"taskId":178,"taskName":"阶段一","stageSum":1,"stageCurrent":1},
                {"taskId":179,"taskName":"阶段二","stageSum":1,"stageCurrent":1}
            ]}
        ]}}"#;
        let after_78 = r#"{"code":0,"data":{"list":[
            {"masterTaskId":78,"taskName":"移动端-每日任务","taskDetailList":[
                {"taskId":201,"taskName":"浏览","stageSum":1,"stageCurrent":1}
            ]}
        ]}}"#;

        let harness = Harness::new(vec![
            json(list),
            json(r#"{"code":0,"data":{"addScore":80,"taskName":"阶段一"}}"#),
            json(after_52),
            json(r#"{"code":0,"data":{"addScore":80,"taskName":"阶段二"}}"#),
            json(after_52),
            json(r#"{"code":0,"data":{"addScore":80,"taskName":"浏览"}}"#),
            json(after_78),
            json(r#"{"data":{"score":8000}}"#),
        ]);

        let mut c = cred();
        let phone = c.phone.clone();
        let mut session = Session::new(&mut c, || 0).with_sleeper(harness.sleeper());
        let game = GameClient::new(&mut session).with_transport(harness.transport());
        let mut runner = ClaimRunner::new(
            phone,
            game,
            Arc::new(|| Duration::from_secs(INTERVAL_SECS)),
        )
        .with_waiter(harness.waiter());

        let outcome = runner.run();

        assert!(outcome.completed, "failure={:?}", outcome.failure);
        assert!(!outcome.already_done);
        assert!(!outcome.cancelled);
        assert_eq!(outcome.claimed, 3);
        assert_eq!(outcome.total, 3);
        assert_eq!(outcome.added_minutes, 3);
        assert_eq!(outcome.balance_minutes, 100.0);
        assert_eq!(
            outcome.stages,
            vec![
                StageOutcome {
                    master_task_id: 52,
                    task_id: 178,
                    name: "点广告得时长·阶段一".to_string(),
                    claimed: 1,
                    sum: 1,
                    added_minutes: 1,
                    refused_code: None,
                },
                StageOutcome {
                    master_task_id: 52,
                    task_id: 179,
                    name: "点广告得时长·阶段二".to_string(),
                    claimed: 1,
                    sum: 1,
                    added_minutes: 1,
                    refused_code: None,
                },
                StageOutcome {
                    master_task_id: 78,
                    task_id: 201,
                    name: "每日任务·浏览".to_string(),
                    claimed: 1,
                    sum: 1,
                    added_minutes: 1,
                    refused_code: None,
                },
            ]
        );

        // 循环顺序：list → push → 对账 → push → 对账 → push → 对账 → 钱包
        assert_eq!(
            harness.methods(),
            vec!["GET", "POST", "GET", "POST", "GET", "POST", "GET", "GET"]
        );
        assert!(harness.urls()[0].contains("mobileGLTaskList"));
        assert!(harness.urls()[1].contains("mobileGLTaskPush"));
        assert!(harness.urls()[6].contains("mobileGLTaskList"));
        assert!(harness.urls()[7].contains("get_user_wallet"));

        // 假等待：每阶段 5s 间隔 + 3s 对账，共三轮；无任何重试退避
        assert_eq!(harness.waits(), vec![5, 3, 5, 3, 5, 3]);
    }

    /// 一次 5xx：旁路对账发现服务端已记账 → 不重发、不进入重试退避，最终按成功收口。
    #[test]
    fn claim_5xx_recovers_via_side_reconcile() {
        let list = r#"{"code":0,"data":{"list":[
            {"masterTaskId":52,"taskName":"移动端-点广告得时长","taskDetailList":[
                {"taskId":178,"taskName":"阶段一","stageSum":1,"stageCurrent":0}
            ]}
        ]}}"#;
        let committed = r#"{"code":0,"data":{"list":[
            {"masterTaskId":52,"taskName":"移动端-点广告得时长","taskDetailList":[
                {"taskId":178,"taskName":"阶段一","stageSum":1,"stageCurrent":1}
            ]}
        ]}}"#;

        let harness = Harness::new(vec![
            json(list),
            status(502),  // push 第一次：5xx，结果未知
            json(committed), // 旁路对账：服务端已记账
            json(committed), // 阶段结束对账
            json(r#"{"data":{"score":4000}}"#),
        ]);

        let mut c = cred();
        let phone = c.phone.clone();
        let mut session = Session::new(&mut c, || 3).with_sleeper(harness.sleeper());
        let game = GameClient::new(&mut session).with_transport(harness.transport());
        let mut runner = ClaimRunner::new(
            phone,
            game,
            Arc::new(|| Duration::from_secs(INTERVAL_SECS)),
        )
        .with_waiter(harness.waiter());

        let outcome = runner.run();

        assert!(outcome.completed, "failure={:?}", outcome.failure);
        assert_eq!(outcome.claimed, 1);
        assert_eq!(outcome.added_minutes, 0, "已发放但分值未知");
        assert_eq!(outcome.balance_minutes, 50.0);
        let methods = harness.methods();
        assert_eq!(
            methods.iter().filter(|m| *m == "POST").count(),
            1,
            "旁路确认后不得重发，methods={methods:?}"
        );
        assert_eq!(
            harness.waits(),
            vec![5, 3],
            "只有间隔与对账，没有重试退避"
        );
    }

    /// 5xx 未恢复：按重试预算重试（3 次 POST），退避 2s/4s 由注入的 sleeper 记账。
    #[test]
    fn claim_retry_budget_and_backoff_are_injected() {
        let list = r#"{"code":0,"data":{"list":[
            {"masterTaskId":52,"taskName":"移动端-点广告得时长","taskDetailList":[
                {"taskId":178,"taskName":"阶段一","stageSum":1,"stageCurrent":0}
            ]}
        ]}}"#;
        let unchanged = r#"{"code":0,"data":{"list":[
            {"masterTaskId":52,"taskName":"移动端-点广告得时长","taskDetailList":[
                {"taskId":178,"taskName":"阶段一","stageSum":1,"stageCurrent":0}
            ]}
        ]}}"#;
        let committed = r#"{"code":0,"data":{"list":[
            {"masterTaskId":52,"taskName":"移动端-点广告得时长","taskDetailList":[
                {"taskId":178,"taskName":"阶段一","stageSum":1,"stageCurrent":1}
            ]}
        ]}}"#;

        let harness = Harness::new(vec![
            json(list),
            status(503),
            json(unchanged), // 第一次旁路：未推进
            status(503),
            json(unchanged), // 第二次旁路：未推进
            json(r#"{"code":0,"data":{"addScore":80}}"#),
            json(committed),
            json(r#"{"data":{"score":4080}}"#),
        ]);

        let mut c = cred();
        let phone = c.phone.clone();
        let mut session = Session::new(&mut c, || 2).with_sleeper(harness.sleeper());
        let game = GameClient::new(&mut session).with_transport(harness.transport());
        let mut runner = ClaimRunner::new(
            phone,
            game,
            Arc::new(|| Duration::from_secs(INTERVAL_SECS)),
        )
        .with_waiter(harness.waiter());

        let outcome = runner.run();

        assert!(outcome.completed, "failure={:?}", outcome.failure);
        assert_eq!(
            harness.methods().iter().filter(|m| *m == "POST").count(),
            3,
            "retry_count=2 → 共 3 次尝试"
        );
        assert_eq!(
            harness.waits(),
            vec![5, 2, 4, 3],
            "间隔 5s、退避 2s/4s、对账 3s 依次发生"
        );
    }

    /// 致命失败：push 返回 NeedRelogin → 整号立即停止，不再 settle、不再推下一个阶段。
    #[test]
    fn fatal_push_failure_stops_account() {
        struct FatalGame {
            pushes: i32,
        }
        impl GameApi for FatalGame {
            fn get_task_stages(
                &mut self,
                _master_task_id: Option<i32>,
                _require_non_empty: bool,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<Vec<TaskStage>> {
                ok_list(vec![
                    stage(52, 178, "点广告得时长·阶段一", 0, 2),
                    stage(52, 179, "点广告得时长·阶段二", 0, 1),
                ])
            }

            fn get_wallet_score(
                &mut self,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<i64> {
                GlaccCallResult::success(0)
            }

            fn push_task(
                &mut self,
                _master_task_id: i32,
                _task_id: i32,
                _local_current: i32,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<PushResult> {
                self.pushes += 1;
                GlaccCallResult::fail(GlaccFailReason::NeedRelogin, "登录已过期")
            }
        }

        let waits = Arc::new(Mutex::new(Vec::new()));
        struct W(Arc<Mutex<Vec<u64>>>);
        impl Waiter for W {
            fn wait(&self, d: Duration) -> bool {
                self.0.lock().unwrap().push(d.as_secs());
                true
            }
        }

        let mut runner = ClaimRunner::new(
            "13800138000",
            FatalGame { pushes: 0 },
            Arc::new(|| Duration::from_secs(INTERVAL_SECS)),
        )
        .with_waiter(Arc::new(W(waits.clone())));

        let outcome = runner.run();
        assert!(!outcome.completed);
        assert_eq!(
            outcome.failure.as_deref(),
            Some("登录已过期，需要重新短信登录")
        );
        assert_eq!(outcome.claimed, 0);
        assert_eq!(outcome.total, 0, "失败路径与旧版一致：claimed/total 归零");
        assert_eq!(outcome.stages.len(), 2, "失败时仍保留阶段快照");
        assert_eq!(outcome.stages[0].claimed, 0);
        assert_eq!(*waits.lock().unwrap(), vec![5], "只等了一次间隔，无对账等待");
        // game 已被 runner 持有，push 次数通过静态计数不便取；这里用阶段快照断言未推进
        assert!(outcome.stages.iter().all(|s| s.claimed == 0));
    }

    /// 取消：等待中被标志唤醒 → 立即返回 cancelled，且不再发起 push。
    #[test]
    fn cancel_during_wait_exits_immediately() {
        struct WaitingGame {
            pushes: Arc<Mutex<i32>>,
            list_calls: Arc<Mutex<i32>>,
        }
        impl GameApi for WaitingGame {
            fn get_task_stages(
                &mut self,
                _master_task_id: Option<i32>,
                _require_non_empty: bool,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<Vec<TaskStage>> {
                *self.list_calls.lock().unwrap() += 1;
                ok_list(vec![stage(52, 178, "阶段一", 0, 3)])
            }

            fn get_wallet_score(
                &mut self,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<i64> {
                GlaccCallResult::success(0)
            }

            fn push_task(
                &mut self,
                _master_task_id: i32,
                _task_id: i32,
                _local_current: i32,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<PushResult> {
                *self.pushes.lock().unwrap() += 1;
                ok_push(80)
            }
        }

        /// 第一次等待就请求取消并返回 false（模拟用户点"停止"）。
        struct CancelOnFirstWait(Arc<AtomicBool>);
        impl Waiter for CancelOnFirstWait {
            fn wait(&self, _d: Duration) -> bool {
                self.0.store(true, Ordering::SeqCst);
                false
            }
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let pushes = Arc::new(Mutex::new(0));
        let list_calls = Arc::new(Mutex::new(0));
        let mut runner = ClaimRunner::new(
            "13800138000",
            WaitingGame {
                pushes: pushes.clone(),
                list_calls: list_calls.clone(),
            },
            Arc::new(|| Duration::from_secs(INTERVAL_SECS)),
        )
        .with_waiter(Arc::new(CancelOnFirstWait(cancel.clone())));

        let outcome = runner.run();
        assert!(outcome.cancelled);
        assert!(!outcome.completed);
        assert_eq!(*pushes.lock().unwrap(), 0, "取消后不得再推送");
        assert_eq!(*list_calls.lock().unwrap(), 1);
    }

    /// 取消发生在开始之前：一次请求都不发。
    #[test]
    fn cancel_before_start_skips_all_network() {
        struct PanicGame;
        impl GameApi for PanicGame {
            fn get_task_stages(
                &mut self,
                _master_task_id: Option<i32>,
                _require_non_empty: bool,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<Vec<TaskStage>> {
                panic!("取消后不应发起任何请求");
            }
            fn get_wallet_score(
                &mut self,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<i64> {
                panic!("取消后不应发起任何请求");
            }
            fn push_task(
                &mut self,
                _master_task_id: i32,
                _task_id: i32,
                _local_current: i32,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<PushResult> {
                panic!("取消后不应发起任何请求");
            }
        }

        let mut runner = ClaimRunner::new(
            "13800138000",
            PanicGame,
            Arc::new(|| Duration::from_secs(1)),
        );
        runner.cancel();
        let outcome = runner.run();
        assert!(outcome.cancelled);
        assert_eq!(outcome.claimed, 0);
    }

    /// 并发调用：共享串行门闸时第二个领取被直接拒绝，且不会触发任何请求。
    #[test]
    fn concurrent_run_is_rejected_by_serial_gate() {
        struct BlockingGame {
            entered: Arc<Barrier>,
            release: Arc<(Mutex<bool>, Condvar)>,
        }
        impl GameApi for BlockingGame {
            fn get_task_stages(
                &mut self,
                _master_task_id: Option<i32>,
                _require_non_empty: bool,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<Vec<TaskStage>> {
                self.entered.wait();
                let (lock, cv) = &*self.release;
                let mut done = lock.lock().unwrap();
                while !*done {
                    done = cv.wait(done).unwrap();
                }
                ok_list(vec![stage(52, 178, "阶段一", 1, 1)])
            }

            fn get_wallet_score(
                &mut self,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<i64> {
                GlaccCallResult::success(80)
            }

            fn push_task(
                &mut self,
                _master_task_id: i32,
                _task_id: i32,
                _local_current: i32,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<PushResult> {
                panic!("已完成后不应推送");
            }
        }

        struct CountingGame(Arc<Mutex<i32>>);
        impl GameApi for CountingGame {
            fn get_task_stages(
                &mut self,
                _master_task_id: Option<i32>,
                _require_non_empty: bool,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<Vec<TaskStage>> {
                *self.0.lock().unwrap() += 1;
                ok_list(vec![stage(52, 178, "阶段一", 1, 1)])
            }
            fn get_wallet_score(
                &mut self,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<i64> {
                GlaccCallResult::success(80)
            }
            fn push_task(
                &mut self,
                _master_task_id: i32,
                _task_id: i32,
                _local_current: i32,
                _on_retry: Option<&mut dyn FnMut(i32, Duration)>,
            ) -> GlaccCallResult<PushResult> {
                ok_push(80)
            }
        }

        let gate = new_serial_gate();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new((Mutex::new(false), Condvar::new()));

        let mut slow = ClaimRunner::new(
            "13800138000",
            BlockingGame {
                entered: entered.clone(),
                release: release.clone(),
            },
            Arc::new(|| Duration::from_secs(0)),
        )
        .with_gate(gate.clone());
        let slow_thread = std::thread::spawn(move || slow.run());

        entered.wait(); // 慢任务已取得门闸并进入请求

        let hits = Arc::new(Mutex::new(0));
        let mut fast = ClaimRunner::new(
            "13900139000",
            CountingGame(hits.clone()),
            Arc::new(|| Duration::from_secs(0)),
        )
        .with_gate(gate.clone());
        let rejected = fast.run();

        assert!(!rejected.completed);
        assert!(!rejected.cancelled);
        assert!(
            rejected
                .failure
                .as_deref()
                .unwrap_or("")
                .contains("串行"),
            "failure={:?}",
            rejected.failure
        );
        assert_eq!(*hits.lock().unwrap(), 0, "被拒绝的领取不得发起任何请求");

        {
            let (lock, cv) = &*release;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        let slow_outcome = slow_thread.join().unwrap();
        assert!(slow_outcome.completed, "慢任务应正常完成");
        assert!(slow_outcome.already_done);
    }

    /// 工具函数与旧版逐值一致：脱敏、80:1 换算、银行家舍入。
    #[test]
    fn helpers_match_legacy() {
        assert_eq!(mask_phone("13800138000"), "13******000");
        assert_eq!(mask_phone("12345"), "12345");
        assert_eq!(mask_phone(""), "");
        assert_eq!(score_to_minutes(8000), 100.0);
        assert_eq!(score_to_minutes_rounded(80), 1);
        // .NET Math.Round：0.5→0, 1.5→2, 2.5→2（Rust 的 round 会把 0.5→1，此处必须取偶）
        assert_eq!(score_to_minutes_rounded(40), 0);
        assert_eq!(score_to_minutes_rounded(120), 2);
        assert_eq!(score_to_minutes_rounded(200), 2);
        assert_eq!(score_to_minutes_rounded(280), 4);
    }
}
