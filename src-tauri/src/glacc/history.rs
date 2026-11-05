// 最近领取历史：<数据根目录>/history.json。
// 结构：{"2026-10-18": {"addedMinutes": 40, "completed": false}, ...}（BTreeMap，按日期有序）
// 纯观测数据：只在领取收尾处写一次（界面实例与 --scheduled 实例共用 record_from_outcome），
// 不参与任何领取判断；文件缺失或损坏一律当作"无记录"处理。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store;

/// 单日记录。camelCase 与前端 HistoryDay 对齐。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryDay {
    /// yyyy-MM-dd（本地日期）
    pub date: String,
    /// 当日累计获得的时长（分钟）
    pub added_minutes: i32,
    /// 当日是否所有任务完成（无失败、无跳过且每号都领满）
    pub completed: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DayRecord {
    added_minutes: i32,
    completed: bool,
}

/// 最多保留的天数：防止 history.json 无限膨胀。
const MAX_DAYS: usize = 30;

fn file_path(root: &Path) -> PathBuf {
    root.join("history.json")
}

/// 本地今天，yyyy-MM-dd。复用 diag 的本地时钟（"yyyy-MM-dd HH:mm:ss.fff" 取前 10 位），零新依赖。
pub fn today() -> String {
    super::diag::local_timestamp()[..10].to_string()
}

fn load_in(root: &Path) -> BTreeMap<String, DayRecord> {
    match std::fs::read_to_string(file_path(root)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => BTreeMap::new(),
    }
}

fn save_in(root: &Path, days: &BTreeMap<String, DayRecord>) {
    if let Ok(text) = serde_json::to_string_pretty(days) {
        if let Err(e) = std::fs::write(file_path(root), text) {
            super::diag::warn(&format!("写入领取历史失败：{e}"));
        }
    }
}

/// 记录一天：时长累加、完成标记只升不降（一天内多轮领取取更优结果）。
fn record_in(root: &Path, date: &str, added_minutes: i32, completed: bool) {
    let mut days = load_in(root);
    let entry = days.entry(date.to_string()).or_insert(DayRecord {
        added_minutes: 0,
        completed: false,
    });
    entry.added_minutes += added_minutes.max(0);
    if completed {
        entry.completed = true;
    }
    while days.len() > MAX_DAYS {
        match days.keys().next().cloned() {
            Some(oldest) => {
                days.remove(&oldest);
            }
            None => break,
        }
    }
    save_in(root, &days);
}

/// 记录今天（本地日期）。
pub fn record(added_minutes: i32, completed: bool) {
    record_in(&store::data_root(), &today(), added_minutes, completed);
}

/// 近 n 天记录，按日期升序。文件缺失/损坏时返回空。
pub fn last_days(n: usize) -> Vec<HistoryDay> {
    last_days_in(&store::data_root(), n)
}

fn last_days_in(root: &Path, n: usize) -> Vec<HistoryDay> {
    let days = load_in(root);
    let all: Vec<HistoryDay> = days
        .into_iter()
        .map(|(date, r)| HistoryDay {
            date,
            added_minutes: r.added_minutes,
            completed: r.completed,
        })
        .collect();
    let start = all.len().saturating_sub(n);
    all[start..].to_vec()
}

/// 领取收尾统一入口：整轮跑完（未被取消）且"全部完成或拿到了时长"才落一条。
/// completed = 无失败、无跳过且每个号都领满自己的任务数。
pub fn record_from_outcome(outcome: &super::sweep::SweepOutcome) {
    if outcome.cancelled {
        return;
    }
    let all_done = outcome.failed == 0
        && outcome.ran > 0
        && outcome
            .entries
            .iter()
            .all(|e| e.state == super::sweep::SweepState::Done && (e.total == 0 || e.claimed >= e.total));
    if outcome.total_added_minutes > 0 || all_done {
        record(outcome.total_added_minutes, all_done);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("glacc-history-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp root");
        dir
    }

    #[test]
    fn record_accumulates_and_keeps_better_completed() {
        let root = temp_root("accumulate");
        record_in(&root, "2026-10-18", 20, false);
        record_in(&root, "2026-10-18", 22, false);
        record_in(&root, "2026-10-18", 0, true);
        let days = last_days_in(&root, 7);
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].date, "2026-10-18");
        assert_eq!(days[0].added_minutes, 42);
        assert!(days[0].completed);
    }

    #[test]
    fn last_days_sorted_ascending_limit() {
        let root = temp_root("sorted");
        for (i, d) in ["2026-10-14", "2026-10-16", "2026-10-15"].iter().enumerate() {
            record_in(&root, d, (i as i32 + 1) * 10, false);
        }
        let days = last_days_in(&root, 2);
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].date, "2026-10-15");
        assert_eq!(days[1].date, "2026-10-16");
    }

    #[test]
    fn prune_keeps_most_recent_30() {
        let root = temp_root("prune");
        // 写 32 天：2026-08-01 .. 2026-09-02（i=0..31）
        for i in 0..32 {
            let date = format!("2026-{:02}-{:02}", 8 + i / 30, 1 + (i % 30));
            record_in(&root, &date, 10, i == 31);
        }
        let days = last_days_in(&root, 100);
        assert_eq!(days.len(), MAX_DAYS);
        assert_eq!(days.last().expect("has last").date, "2026-09-02");
        assert!(days.last().expect("has last").completed);
        assert_eq!(days.first().expect("has first").date, "2026-08-03");
        assert!(!days.first().expect("has first").completed);
    }

    #[test]
    fn missing_file_means_empty() {
        let root = temp_root("missing");
        assert!(last_days_in(&root, 7).is_empty());
    }
}
