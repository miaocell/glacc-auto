//! 真实 Windows 计划任务注册 / 查询 / 注销往返（默认 ignored，手动触发）。
//!
//! 这是唯一会触碰真实系统计划任务的测试：它向 Windows 注册 `glacc-auto-claim`，
//! 用 schtasks /Query 回读并逐项校验任务定义，最后注销并确认任务已消失。
//! 无论断言成败，Drop 守卫都会注销，不留垃圾任务。
//!
//! 运行：
//!   cd src-tauri && cargo test --test schedule_live -- --ignored --nocapture
//!
//! 保护：若系统里已存在同名任务（用户正在用定时领取），直接跳过，绝不覆盖。

use glacc_auto_lib::glacc::schedule;

/// 无论测试如何结束都注销任务。
struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        match schedule::unregister() {
            Ok(()) => eprintln!("[cleanup] 已注销 glacc-auto-claim"),
            Err(e) => eprintln!("[cleanup] 注销失败：{e}"),
        }
    }
}

#[test]
#[ignore = "会真实注册/注销系统计划任务 glacc-auto-claim，手动运行"]
fn real_scheduled_task_roundtrip() {
    if let Ok(Some(existing)) = schedule::query_snapshot() {
        eprintln!(
            "系统里已存在同名任务（{} {}），跳过本次真实注册以免覆盖",
            existing.command,
            existing.start_time.as_hhmm()
        );
        return;
    }

    let exe = std::env::current_exe().expect("取不到当前可执行文件路径");
    // 凌晨 3:07：测试期间不会被触发（任务到点会以 --scheduled 拉起命令）
    let time = schedule::TimeOfDay::new(3, 7).expect("时刻合法");
    let _cleanup = Cleanup;

    schedule::register(&exe, time).expect("注册计划任务失败");

    let snapshot = schedule::query_snapshot()
        .expect("查询失败")
        .expect("刚注册的任务必须存在");
    eprintln!(
        "回读任务：command={} arguments={} time={} 不限时={}",
        snapshot.command,
        snapshot.arguments,
        snapshot.start_time.as_hhmm(),
        snapshot.no_execution_time_limit
    );
    assert!(
        schedule::matches_expectation(&snapshot, time, &exe),
        "任务定义与预期不符：{snapshot:?}"
    );

    schedule::unregister().expect("注销计划任务失败");
    let after = schedule::query_snapshot().expect("注销后查询失败");
    assert!(after.is_none(), "注销后任务不应存在：{after:?}");
    eprintln!("往返完成：注册 → 回读校验 → 注销，系统里已无 glacc-auto-claim");
}
