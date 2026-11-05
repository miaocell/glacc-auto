// 多账号串行领取编排，移植自旧版
// src\GlaccAuto.Core\Claiming\AccountSweep.cs（Run / RunQueueAsync）。
//
// 旧版要点（本次只读核实的结论）：
// - 遍历顺序 = AccountStore.ListAccounts() 的返回序（最近使用倒序），队列在启动时快照一次，
//   中途增删账号不影响本轮，避免同一个号跑两遍或漏号。
// - 号间固定停顿 5 秒，位置在每轮结束处、且只在"还有下一个号且未取消"时执行。
// - 单号失败不中断整轮：记录状态后继续下一个（Skip 同理）。
// - 全局取消由 CancellationTokenSource 表达，循环顶部检查，等待也带 token。
//
// Rust 侧映射：同步循环 + 可注入 Waiter（不引入 tokio）；取消用 Arc<AtomicBool>，
// 与 ClaimRunner 共用同一个标志，当前账号在下一个等待/检查点退出，后续账号不再执行。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::glacc::claim::{
    mask_phone, new_serial_gate, CancellableWaiter, ClaimOutcome, ClaimRunner, Reporter,
    SerialGate, Waiter,
};
use crate::glacc::credentials::Credentials;
use crate::glacc::game::GameClient;
use crate::glacc::result::GlaccFailReason;
use crate::glacc::session::Session;
use crate::glacc::store;

/// 号间固定停顿（旧版 RunQueueAsync 的 TimeSpan.FromSeconds(5)）。
pub const ACCOUNT_GAP: Duration = Duration::from_secs(5);

/// 队列中的一个账号（键 + 手机号数字串；手机号仅用于脱敏展示，不落日志原文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepAccount {
    pub key: String,
    pub phone_digits: String,
}

impl From<store::Account> for SweepAccount {
    fn from(a: store::Account) -> Self {
        Self {
            key: a.key,
            phone_digits: a.phone_digits,
        }
    }
}

impl SweepAccount {
    /// 与旧版 RunQueueAsync 的取名规则一致：11 位才脱敏，否则显示"未知账号"。
    pub fn label(&self) -> String {
        if self.phone_digits.chars().count() == 11 {
            mask_phone(&self.phone_digits)
        } else {
            "未知账号".to_string()
        }
    }
}

/// 单号本轮状态（旧版 SweepEntry.State 的字符串值 → 枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepState {
    Pending,
    Running,
    Done,
    Failed,
    /// 未登录/需重新登录/被取消：不算失败，不阻断整轮。
    Skipped,
}

/// 单号本轮结果快照（供界面轮询展示；本步不接 UI，只提供数据结构）。
#[derive(Debug, Clone, PartialEq)]
pub struct SweepEntry {
    pub key: String,
    pub label: String,
    pub state: SweepState,
    pub claimed: i32,
    pub total: i32,
    pub added_minutes: i32,
    pub balance_minutes: f64,
    pub message: Option<String>,
}

/// 一轮扫描的汇总：逐号结果 + 聚合计数。
#[derive(Debug, Clone, PartialEq)]
pub struct SweepOutcome {
    pub entries: Vec<SweepEntry>,
    pub cancelled: bool,
    /// 实际执行过领取的号数（不含跳过）。
    pub ran: usize,
    pub skipped: usize,
    pub failed: usize,
    pub total_claimed: i32,
    pub total_added_minutes: i32,
}

/// 单号执行结果：正常跑完（含失败）或直接跳过（无凭证/登录态不可用）。
#[derive(Debug, Clone, PartialEq)]
pub enum AccountAttempt {
    Ran(ClaimOutcome),
    Skipped(String),
}

/// 扫描层的进度事件（步 7 新增）：界面据此实时知道"正在领哪个号、进度如何"。
/// 事件按队列下标携带位置，界面不需要自己猜当前是第几个号。
#[derive(Debug, Clone, PartialEq)]
pub enum SweepEvent {
    /// 某号开始执行
    Started { index: usize, key: String, label: String },
    /// 某号结束（含被取消而批量跳过的号）
    Finished { index: usize, entry: SweepEntry },
}

/// 扫描进度回调（默认空实现；界面接线时传入）。
pub type SweepReporter = Arc<dyn Fn(&SweepEvent) + Send + Sync>;

fn noop_sweep_reporter() -> SweepReporter {
    Arc::new(|_| {})
}

/// 单号执行器抽象：生产实现走真实网络，测试用 Mock 记录调用顺序、不触网。
pub trait AccountClaimant {
    fn claim(&mut self, account: &SweepAccount, cancel: &Arc<AtomicBool>) -> AccountAttempt;
}

/// 生产实现：读凭据 → 需要时续期 → ClaimRunner 跑一轮。
pub struct LiveClaimant {
    retry_count: i32,
    interval: Arc<dyn Fn() -> Duration + Send + Sync>,
    gate: SerialGate,
    /// 逐阶段/逐次领取的进度回调（界面实时进度用；默认空实现）
    reporter: Reporter,
}

impl LiveClaimant {
    pub fn new(retry_count: i32, interval: Arc<dyn Fn() -> Duration + Send + Sync>) -> Self {
        Self {
            retry_count,
            interval,
            gate: new_serial_gate(),
            reporter: Arc::new(|_| {}),
        }
    }

    /// 与其他调用方共享串行门闸（全局同一时刻只允许一个领取任务）。
    pub fn with_gate(mut self, gate: SerialGate) -> Self {
        self.gate = gate;
        self
    }

    /// 接上领取进度回调：把"阶段一：+2 分钟（3/48）"这类事件实时送到界面。
    pub fn with_reporter(mut self, reporter: Reporter) -> Self {
        self.reporter = reporter;
        self
    }
}

impl AccountClaimant for LiveClaimant {
    fn claim(&mut self, account: &SweepAccount, cancel: &Arc<AtomicBool>) -> AccountAttempt {
        let mut cred = store::load(&account.key);
        // 旧版 AccountSweep：两者皆空 = 本机无登录凭证，跳过而非失败
        if !cred.has_token() && !cred.has_refresh_token() {
            return AccountAttempt::Skipped("本机无登录凭证，需先短信登录".to_string());
        }
        let phone = cred.phone.clone();
        let retry = self.retry_count;
        let mut session = Session::new(&mut cred, move || retry);
        if session.jwt_needs_refresh() {
            let r = session.ensure_jwt(false);
            if !r.ok {
                // 旧版 Skip：登录态不可用（过期且无法续期）→ 跳过该号继续下一个
                return AccountAttempt::Skipped(r.error);
            }
        }
        let game = GameClient::new(&mut session);
        let mut runner = ClaimRunner::new(phone, game, self.interval.clone())
            .with_cancel_flag(cancel.clone())
            .with_gate(self.gate.clone())
            .with_reporter(self.reporter.clone());
        AccountAttempt::Ran(runner.run())
    }
}

/// 多账号串行扫描。
pub struct AccountSweep<C: AccountClaimant> {
    queue: Vec<SweepAccount>,
    claimant: C,
    gap: Duration,
    waiter: Arc<dyn Waiter>,
    cancel: Arc<AtomicBool>,
    reporter: SweepReporter,
}

impl<C: AccountClaimant> AccountSweep<C> {
    /// queue 顺序即执行顺序（生产调用方传 `plan_from_store()` 的返回序）。
    /// 账号上限 3 硬拦截：超出部分直接截断，绝不因外部传入更多而越界。
    pub fn new(queue: Vec<SweepAccount>, claimant: C) -> Self {
        let mut queue = queue;
        if queue.len() > store::MAX_ACCOUNTS {
            store::log(&format!(
                "账号队列 {} 个超出上限 {}，已截断",
                queue.len(),
                store::MAX_ACCOUNTS
            ));
            queue.truncate(store::MAX_ACCOUNTS);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        Self {
            queue,
            claimant,
            gap: ACCOUNT_GAP,
            waiter: Arc::new(CancellableWaiter::new(cancel.clone())),
            cancel,
            reporter: noop_sweep_reporter(),
        }
    }

    /// 注入扫描进度回调（界面实时显示逐号状态；默认空实现）。
    pub fn with_reporter(mut self, reporter: SweepReporter) -> Self {
        self.reporter = reporter;
        self
    }

    /// 注入等待器（测试用：记录而不真睡）。
    pub fn with_waiter(mut self, waiter: Arc<dyn Waiter>) -> Self {
        self.waiter = waiter;
        self
    }

    /// 号间停顿（旧版固定 5 秒；保留可配置以便测试断言与后续设置项）。
    pub fn with_gap(mut self, gap: Duration) -> Self {
        self.gap = gap;
        self
    }

    /// 复用外部取消标志（界面"停止"与调度层共用）。
    pub fn with_cancel_flag(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.waiter = Arc::new(CancellableWaiter::new(cancel.clone()));
        self.cancel = cancel;
        self
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    /// 请求停止：当前号在下一个检查点退出，后续号不再执行。
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// 本轮队列快照（顺序即执行顺序）。
    pub fn queue(&self) -> &[SweepAccount] {
        &self.queue
    }

    /// 执行整轮：严格串行，逐号跑完再进下一个。
    pub fn run(&mut self) -> SweepOutcome {
        let mut entries: Vec<SweepEntry> = self
            .queue
            .iter()
            .map(|a| SweepEntry {
                key: a.key.clone(),
                label: a.label(),
                state: SweepState::Pending,
                claimed: 0,
                total: 0,
                added_minutes: 0,
                balance_minutes: 0.0,
                message: None,
            })
            .collect();

        for i in 0..self.queue.len() {
            if self.cancel.load(Ordering::SeqCst) {
                skip_rest(&mut entries[i..], i, "已取消", &self.reporter);
                break;
            }

            let account = self.queue[i].clone();
            entries[i].state = SweepState::Running;
            store::log(&format!(
                "账号 {}（{}/{}）开始领取",
                entries[i].label,
                i + 1,
                self.queue.len()
            ));
            (self.reporter)(&SweepEvent::Started {
                index: i,
                key: account.key.clone(),
                label: entries[i].label.clone(),
            });

            let attempt = self.claimant.claim(&account, &self.cancel);
            let cancelled_now = self.cancel.load(Ordering::SeqCst);

            match attempt {
                AccountAttempt::Skipped(why) => {
                    entries[i].state = SweepState::Skipped;
                    entries[i].message = Some(why.clone());
                    store::log(&format!("账号 {} 跳过：{why}", entries[i].label));
                }
                AccountAttempt::Ran(outcome) => {
                    entries[i].claimed = outcome.claimed;
                    entries[i].total = outcome.total;
                    entries[i].added_minutes = outcome.added_minutes;
                    entries[i].balance_minutes = outcome.balance_minutes;

                    if outcome.cancelled {
                        entries[i].state = SweepState::Skipped;
                        entries[i].message = Some("已取消".to_string());
                    } else if needs_relogin(&outcome) {
                        entries[i].state = SweepState::Skipped;
                        entries[i].message = outcome.failure.clone();
                        store::log(&format!(
                            "账号 {} 跳过：{}",
                            entries[i].label,
                            outcome.failure.as_deref().unwrap_or("登录态不可用")
                        ));
                    } else if outcome.completed || outcome.already_done {
                        entries[i].state = SweepState::Done;
                        entries[i].message = Some(outcome.failure.clone().unwrap_or_else(|| {
                            format!(
                                "本次 +{} 分钟，余额 {} 分钟",
                                outcome.added_minutes,
                                outcome.balance_minutes.round() as i64
                            )
                        }));
                        store::log(&format!(
                            "账号 {} 完成：{}/{}，+{} 分钟",
                            entries[i].label, outcome.claimed, outcome.total, outcome.added_minutes
                        ));
                    } else {
                        // 单号失败不中断整轮：记录后继续下一个（旧版语义）
                        entries[i].state = SweepState::Failed;
                        entries[i].message = outcome.failure.clone();
                        store::log(&format!(
                            "账号 {} 未完成：{}",
                            entries[i].label,
                            outcome.failure.as_deref().unwrap_or("未知原因")
                        ));
                    }
                }
            }

            // 本号已有最终状态：立刻把结果推到界面（不等整轮结束）
            (self.reporter)(&SweepEvent::Finished {
                index: i,
                entry: entries[i].clone(),
            });

            if cancelled_now {
                // 当前号已因取消退出：剩余号一律跳过，也不再有号间停顿
                skip_rest(&mut entries[i + 1..], i + 1, "已取消", &self.reporter);
                break;
            }

            // 号间停顿：只在还有下一个号时执行（旧版同一个条件）
            if i + 1 < self.queue.len() && !self.waiter.wait(self.gap) {
                skip_rest(&mut entries[i + 1..], i + 1, "已取消", &self.reporter);
                break;
            }
        }

        let cancelled = self.cancel.load(Ordering::SeqCst);
        // 计数一律由最终状态导出：末尾批量标记跳过的号也要计入，避免两套口径打架
        let ran = entries
            .iter()
            .filter(|e| matches!(e.state, SweepState::Done))
            .count();
        let skipped = entries
            .iter()
            .filter(|e| matches!(e.state, SweepState::Skipped))
            .count();
        let failed = entries
            .iter()
            .filter(|e| matches!(e.state, SweepState::Failed))
            .count();
        let outcome = SweepOutcome {
            total_claimed: entries
                .iter()
                .filter(|e| matches!(e.state, SweepState::Done))
                .map(|e| e.claimed)
                .sum(),
            total_added_minutes: entries
                .iter()
                .filter(|e| matches!(e.state, SweepState::Done))
                .map(|e| e.added_minutes)
                .sum(),
            entries,
            cancelled,
            ran,
            skipped,
            failed,
        };
        store::log(&format!(
            "本轮扫描结束：执行 {}，跳过 {}，失败 {}，+{} 分钟{}",
            outcome.ran,
            outcome.skipped,
            outcome.failed,
            outcome.total_added_minutes,
            if outcome.cancelled { "（已取消）" } else { "" }
        ));
        outcome
    }
}

/// 需要重新登录（或尚未登录）→ 判为跳过而非失败（旧版 Skip 分支）。
fn needs_relogin(outcome: &ClaimOutcome) -> bool {
    matches!(
        outcome.fail_reason,
        Some(GlaccFailReason::NeedRelogin) | Some(GlaccFailReason::NotLoggedIn)
    )
}

/// 批量把剩余待执行的号标为跳过，并逐个把结果推给界面（取消时用）。
/// from 是 rest 首元素在完整 entries 里的下标，保证事件里的 index 始终是队列位置。
fn skip_rest(rest: &mut [SweepEntry], from: usize, why: &str, reporter: &SweepReporter) {
    for (offset, e) in rest.iter_mut().enumerate() {
        if e.state == SweepState::Pending {
            e.state = SweepState::Skipped;
            e.message = Some(why.to_string());
            reporter(&SweepEvent::Finished {
                index: from + offset,
                entry: e.clone(),
            });
        }
    }
}

/// 本轮要跑的账号 = 全部已登录账号（最近使用倒序，已按 store 内置上限截断）。
pub fn plan_from_store() -> Vec<SweepAccount> {
    store::list_accounts()
        .into_iter()
        .take(store::MAX_ACCOUNTS)
        .map(SweepAccount::from)
        .collect()
}

/// 手机号脱敏（透传 claim 的实现，保持单一来源）。
pub fn label_for(phone: &str) -> String {
    mask_phone(&Credentials::phone_digits(phone))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glacc::claim::{StageOutcome, Waiter};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    fn account(n: u8) -> SweepAccount {
        SweepAccount {
            key: format!("key-{n}"),
            phone_digits: format!("1380013800{n}"),
        }
    }

    fn done(claimed: i32, total: i32, added: i32) -> ClaimOutcome {
        ClaimOutcome {
            account_label: "13******000".to_string(),
            completed: true,
            already_done: false,
            claimed,
            total,
            added_minutes: added,
            balance_minutes: 100.0,
            stages: Vec::new(),
            failure: None,
            fail_reason: None,
            cancelled: false,
        }
    }

    fn outcome_with(reason: GlaccFailReason) -> ClaimOutcome {
        ClaimOutcome {
            account_label: "13******000".to_string(),
            completed: false,
            already_done: false,
            claimed: 0,
            total: 0,
            added_minutes: 0,
            balance_minutes: 0.0,
            stages: Vec::new(),
            failure: Some("登录已过期，需要重新短信登录".to_string()),
            fail_reason: Some(reason),
            cancelled: false,
        }
    }

    fn cancelled() -> ClaimOutcome {
        ClaimOutcome {
            account_label: "13******000".to_string(),
            completed: false,
            already_done: false,
            claimed: 1,
            total: 3,
            added_minutes: 1,
            balance_minutes: 0.0,
            stages: vec![StageOutcome {
                master_task_id: 52,
                task_id: 178,
                name: "阶段一".to_string(),
                claimed: 1,
                sum: 3,
                added_minutes: 1,
                refused_code: None,
            }],
            failure: None,
            fail_reason: None,
            cancelled: true,
        }
    }

    /// 脚本化 Mock：按 key 返回预设结果，并记录实际调用顺序。
    struct MockClaimant {
        script: Mutex<VecDeque<(String, AccountAttempt)>>,
        calls: Arc<Mutex<Vec<String>>>,
        /// 在第 N 次调用（1 起）后置位 sweep 传入的取消标志，模拟"领取过程中用户点停止"
        cancel_after: Option<usize>,
    }

    impl MockClaimant {
        fn new(script: Vec<(String, AccountAttempt)>) -> (Self, Arc<Mutex<Vec<String>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    script: Mutex::new(script.into()),
                    calls: calls.clone(),
                    cancel_after: None,
                },
                calls,
            )
        }

        fn cancelling(
            script: Vec<(String, AccountAttempt)>,
            after: usize,
        ) -> (Self, Arc<Mutex<Vec<String>>>) {
            let calls = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    script: Mutex::new(script.into()),
                    calls: calls.clone(),
                    cancel_after: Some(after),
                },
                calls,
            )
        }
    }

    impl AccountClaimant for MockClaimant {
        fn claim(&mut self, account: &SweepAccount, cancel: &Arc<AtomicBool>) -> AccountAttempt {
            let mut calls = self.calls.lock().unwrap();
            calls.push(account.key.clone());
            let n = calls.len();
            drop(calls);

            if self.cancel_after == Some(n) {
                cancel.store(true, Ordering::SeqCst);
            }

            let mut script = self.script.lock().unwrap();
            match script.pop_front() {
                Some((key, attempt)) if key == account.key => attempt,
                Some((key, _)) => panic!("脚本顺序不符：期望 {key}，实际 {}", account.key),
                None => panic!("脚本已耗尽"),
            }
        }
    }

    struct RecordingWaiter {
        waits: Arc<Mutex<Vec<u64>>>,
        cancel_after_wait: Option<(usize, Arc<AtomicBool>)>,
        count: Mutex<usize>,
    }

    impl Waiter for RecordingWaiter {
        fn wait(&self, d: Duration) -> bool {
            self.waits.lock().unwrap().push(d.as_secs());
            let mut count = self.count.lock().unwrap();
            *count += 1;
            if let Some((n, cancel)) = &self.cancel_after_wait {
                if *count == *n {
                    cancel.store(true, Ordering::SeqCst);
                }
            }
            true
        }
    }

    fn recorder() -> (Arc<RecordingWaiter>, Arc<Mutex<Vec<u64>>>) {
        let waits = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(RecordingWaiter {
                waits: waits.clone(),
                cancel_after_wait: None,
                count: Mutex::new(0),
            }),
            waits,
        )
    }

    /// 串行：3 个账号严格按队列顺序执行，中间正好 2 次 5 秒停顿。
    #[test]
    fn serial_order_and_gap() {
        let (claimant, calls) = MockClaimant::new(vec![
            ("key-1".to_string(), AccountAttempt::Ran(done(2, 2, 2))),
            ("key-2".to_string(), AccountAttempt::Ran(done(1, 1, 1))),
            ("key-3".to_string(), AccountAttempt::Ran(done(3, 3, 3))),
        ]);
        let (waiter, waits) = recorder();
        let mut sweep = AccountSweep::new(
            vec![account(1), account(2), account(3)],
            claimant,
        )
        .with_waiter(waiter);

        let out = sweep.run();

        assert_eq!(
            *calls.lock().unwrap(),
            vec!["key-1".to_string(), "key-2".to_string(), "key-3".to_string()],
            "必须严格按队列顺序执行"
        );
        assert_eq!(*waits.lock().unwrap(), vec![5, 5], "3 个号之间正好 2 次 5 秒");
        assert_eq!(out.ran, 3);
        assert_eq!(out.skipped, 0);
        assert_eq!(out.failed, 0);
        assert_eq!(out.total_claimed, 6);
        assert_eq!(out.total_added_minutes, 6);
        assert!(out.entries.iter().all(|e| e.state == SweepState::Done));
        assert!(!out.cancelled);
    }

    /// 第 2 个账号需要重新登录：跳过它，继续执行第 3 个，统计口径正确。
    #[test]
    fn relogin_account_is_skipped_and_sweep_continues() {
        let (claimant, calls) = MockClaimant::new(vec![
            ("key-1".to_string(), AccountAttempt::Ran(done(2, 2, 2))),
            (
                "key-2".to_string(),
                AccountAttempt::Ran(outcome_with(GlaccFailReason::NeedRelogin)),
            ),
            ("key-3".to_string(), AccountAttempt::Ran(done(1, 1, 1))),
        ]);
        let (waiter, waits) = recorder();
        let mut sweep =
            AccountSweep::new(vec![account(1), account(2), account(3)], claimant).with_waiter(waiter);

        let out = sweep.run();

        assert_eq!(
            *calls.lock().unwrap(),
            vec!["key-1".to_string(), "key-2".to_string(), "key-3".to_string()],
            "跳过第 2 个后仍须执行第 3 个"
        );
        assert_eq!(*waits.lock().unwrap(), vec![5, 5]);
        assert_eq!(out.entries[1].state, SweepState::Skipped);
        assert!(out.entries[1]
            .message
            .as_deref()
            .unwrap_or("")
            .contains("重新短信登录"));
        assert_eq!(out.entries[2].state, SweepState::Done);
        assert_eq!(out.ran, 2);
        assert_eq!(out.skipped, 1);
        assert_eq!(out.failed, 0);
        assert_eq!(out.total_claimed, 3, "只累计真正完成的号");
        assert_eq!(out.total_added_minutes, 3);
    }

    /// 单号网络失败：整轮不中断，后续账号照常执行，失败计数正确。
    #[test]
    fn failed_account_does_not_abort_sweep() {
        let (claimant, calls) = MockClaimant::new(vec![
            (
                "key-1".to_string(),
                AccountAttempt::Ran(outcome_with(GlaccFailReason::NoResponse)),
            ),
            ("key-2".to_string(), AccountAttempt::Ran(done(1, 1, 1))),
        ]);
        let (waiter, _) = recorder();
        let mut sweep =
            AccountSweep::new(vec![account(1), account(2)], claimant).with_waiter(waiter);

        let out = sweep.run();
        assert_eq!(calls.lock().unwrap().len(), 2);
        assert_eq!(out.entries[0].state, SweepState::Failed);
        assert_eq!(out.entries[1].state, SweepState::Done);
        assert_eq!(out.failed, 1);
        assert_eq!(out.ran, 1);
    }

    /// 领取中途触发取消：当前号立即退出，后续账号一次都不执行。
    #[test]
    fn cancel_during_account_stops_remaining() {
        let (claimant, calls) = MockClaimant::cancelling(
            vec![
                ("key-1".to_string(), AccountAttempt::Ran(done(1, 1, 1))),
                ("key-2".to_string(), AccountAttempt::Ran(cancelled())),
                ("key-3".to_string(), AccountAttempt::Ran(done(9, 9, 9))),
            ],
            2,
        );
        let (waiter, waits) = recorder();
        let mut sweep = AccountSweep::new(
            vec![account(1), account(2), account(3)],
            claimant,
        )
        .with_waiter(waiter);

        let out = sweep.run();

        assert_eq!(
            *calls.lock().unwrap(),
            vec!["key-1".to_string(), "key-2".to_string()],
            "取消后第 3 个账号不得执行"
        );
        assert_eq!(*waits.lock().unwrap(), vec![5], "取消后不再有号间停顿");
        assert!(out.cancelled);
        assert_eq!(out.entries[1].state, SweepState::Skipped);
        assert_eq!(out.entries[2].state, SweepState::Skipped);
        assert_eq!(out.entries[2].message.as_deref(), Some("已取消"));
        assert_eq!(out.entries[2].claimed, 0);
        assert_eq!(out.ran, 1);
        assert_eq!(out.skipped, 2);
    }

    /// 队列上限 3：传入 5 个只执行前 3 个（硬拦截）。
    #[test]
    fn queue_is_capped_at_three() {
        let mut script = Vec::new();
        for n in 1..=5u8 {
            script.push((
                format!("key-{n}"),
                AccountAttempt::Ran(done(1, 1, 1)),
            ));
        }
        let (claimant, calls) = MockClaimant::new(script);
        let (waiter, _) = recorder();
        let queue: Vec<SweepAccount> = (1..=5u8).map(account).collect();
        let mut sweep = AccountSweep::new(queue, claimant).with_waiter(waiter);

        let out = sweep.run();
        assert_eq!(sweep.queue().len(), store::MAX_ACCOUNTS);
        assert_eq!(calls.lock().unwrap().len(), store::MAX_ACCOUNTS);
        assert_eq!(out.entries.len(), store::MAX_ACCOUNTS);
    }
}
