// 本机诊断日志，逐字移植自旧 Avalonia 版
// src\GlaccAuto.Core\Diagnostics\DiagLog.cs。
//
// 旧版核实结论：
// - 目录：<数据根目录>\logs\（GLACC_AUTO_DATA 可覆盖）
// - 文件名：glacc-auto-yyyyMMdd.log，一天一个文件，按天滚动（不是按大小）
// - 行格式：yyyy-MM-dd HH:mm:ss.fff [LEVEL] message，异常另起一行
// - 写入：进程内 lock + FileMode.Append/FileAccess.Write/FileShare.ReadWrite，
//   UTF-8；shared 写模式是为了让计划任务拉起的第二实例能同写同一文件
// - 清理：Prune() 在启动时调用一次，删除最后写入时间早于 7 天的 glacc-auto-*.log
// - 任何写入/清理失败一律静默（磁盘或权限问题不能影响业务）
//
// Rust 侧差异：时间用 GetLocalTime（Win32）取本地时间，与旧版 DateTime.Now 语义一致；
// 文件追加用 OpenOptions::append，它本身带 FILE_APPEND_DATA，单次 write 在同一进程内由互斥锁串行，
// 跨进程时 Windows 对 append 模式写入是原子的（每行一次 write）。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::glacc::store;

/// 日志保留天数，超期文件在启动时清理。
pub const RETENTION_DAYS: i64 = 7;

/// 进程内写锁：与旧版 lock (Gate) 对应。跨进程由 append 模式 + Windows 的写原子性兜住。
static GATE: Mutex<()> = Mutex::new(());

/// 日志目录。
pub fn directory_path() -> PathBuf {
    store::data_root().join("logs")
}

/// 当前日志文件路径（一天一个文件）。
pub fn current_file_path() -> PathBuf {
    directory_path().join(format!("glacc-auto-{}.log", local_date_stamp()))
}

pub fn info(message: &str) {
    write("INFO", message);
}

pub fn warn(message: &str) {
    write("WARN", message);
}

pub fn error(message: &str) {
    write("ERROR", message);
}

/// 追加一行日志；任何写入失败都静默忽略。
pub fn write(level: &str, message: &str) {
    let line = format!("{} [{}] {}", local_timestamp(), level, message);
    if !should_persist() {
        // 测试构建且未指定隔离目录：只落 stderr，绝不写进真实用户目录。
        // 业务构建（cfg!(test) == false）恒为 true，生产行为零差异。
        eprintln!("[glacc-auto][{level}] {message}");
        return;
    }
    let path = current_file_path();
    // 锁中毒时沿用内部值继续写：日志宁可继续，不该因别处 panic 永久停摆。
    let _guard = GATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            let _ = e; // 静默：业务优先
            return;
        }
    }
    // append 模式：等价旧版 FileMode.Append + FileShare.ReadWrite（不加独占锁，允许第二实例同写）
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            // 单次 write 写入整行（含换行），避免跨进程交错出半行
            let mut buf = String::with_capacity(line.len() + 2);
            buf.push_str(&line);
            buf.push_str("\r\n");
            let _ = f.write_all(buf.as_bytes());
            let _ = f.flush();
        }
        Err(_) => { /* 静默 */ }
    }
}

/// 是否允许落盘。测试构建下只有显式设置 GLACC_AUTO_DATA 才写，
/// 避免未做环境隔离的用例把日志撒进真实 %APPDATA%。
fn should_persist() -> bool {
    if cfg!(test) {
        std::env::var("GLACC_AUTO_DATA")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    } else {
        true
    }
}

/// 清理过期日志（仅本应用日志目录内、文件名匹配 glacc-auto-*.log 的文件）。
/// 与旧版一致：按文件最后写入时间判定，早于 7 天前即删除；失败静默。
pub fn prune() -> usize {
    prune_at_seconds(now_seconds())
}

/// 以给定"当前时间"清理（测试可注入，避免依赖真实时钟）。
/// 返回实际删除的文件数。
pub fn prune_at_seconds(now: i64) -> usize {
    let dir = directory_path();
    let entries = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return 0, // 目录不存在或不可读：静默
    };
    let deadline = now - RETENTION_DAYS * 24 * 3600;
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !(name.starts_with("glacc-auto-") && name.ends_with(".log")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let Some(mtime) = mtime_seconds(&meta) else {
            continue;
        };
        if mtime < deadline && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 文件最后写入时间（Unix 秒）。
fn mtime_seconds(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)
}

fn now_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── 本地时间（Win32 GetLocalTime，零新增依赖）──

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

#[allow(non_snake_case)]
#[link(name = "kernel32")]
extern "system" {
    fn GetLocalTime(lp_system_time: *mut SystemTime);
}

fn local_now() -> SystemTime {
    let mut st = SystemTime::default();
    // SAFETY: GetLocalTime 只写出一个 SYSTEMTIME（16 字节 POD），指针来自本地可写变量。
    unsafe { GetLocalTime(&mut st) };
    st
}

/// yyyyMMdd（日志文件名用）。
fn local_date_stamp() -> String {
    let t = local_now();
    format!("{:04}{:02}{:02}", t.year, t.month, t.day)
}

/// yyyy-MM-dd HH:mm:ss.fff（行首时间戳）。导出文件名也复用它，避免再写一份本地时间代码。
pub fn local_timestamp() -> String {
    let t = local_now();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.year, t.month, t.day, t.hour, t.minute, t.second, t.milliseconds
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glacc::store::ENV_GUARD;
    use std::path::Path;
    use std::sync::{Arc, Barrier};

    struct Isolated {
        dir: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Isolated {
        fn new(tag: &str) -> Isolated {
            let lock = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("glacc-tauri-step2c-diag-{tag}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var("GLACC_AUTO_DATA", &dir);
            Isolated { dir, _lock: lock }
        }
    }

    impl Drop for Isolated {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
            std::env::remove_var("GLACC_AUTO_DATA");
        }
    }

    /// 目录与文件名规则：<data_root>/logs/glacc-auto-yyyyMMdd.log。
    #[test]
    fn paths_follow_legacy_layout() {
        let iso = Isolated::new("paths");
        assert_eq!(directory_path(), iso.dir.join("logs"));
        let name = current_file_path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(name.starts_with("glacc-auto-"), "name={name}");
        assert!(name.ends_with(".log"), "name={name}");
        let stamp = &name["glacc-auto-".len()..name.len() - 4];
        assert_eq!(stamp.len(), 8, "应为 yyyyMMdd：{stamp}");
        assert!(stamp.chars().all(|c| c.is_ascii_digit()));
    }

    /// 行格式：yyyy-MM-dd HH:mm:ss.fff [LEVEL] message。
    #[test]
    fn line_format_matches_legacy() {
        let iso = Isolated::new("format");
        info("你好 hello");
        warn("注意");
        error("出错");

        let text = std::fs::read_to_string(current_file_path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "text={text}");

        for (line, level) in lines.iter().zip(["INFO", "WARN", "ERROR"]) {
            assert!(line.contains(&format!("[{level}] ")), "line={line}");
            // 时间戳部分：19 位日期时间 + .mmm
            let head: String = line.chars().take(23).collect();
            assert_eq!(&head[4..5], "-");
            assert_eq!(&head[7..8], "-");
            assert_eq!(&head[10..11], " ");
            assert_eq!(&head[13..14], ":");
            assert_eq!(&head[16..17], ":");
            assert_eq!(&head[19..20], ".");
            assert!(head[..4].chars().all(|c| c.is_ascii_digit()));
        }
        assert!(lines[0].ends_with("你好 hello"));
        assert!(lines[2].ends_with("出错"));
        assert!(current_file_path().starts_with(&iso.dir));
    }

    /// 并发写入：多线程各写若干行，内容一条不少、每行完整（不交错）。
    #[test]
    fn concurrent_writes_keep_all_lines() {
        let _iso = Isolated::new("concurrent");
        let threads = 8usize;
        let per_thread = 25usize;
        let barrier = Arc::new(Barrier::new(threads));

        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    for i in 0..per_thread {
                        info(&format!("线程{t} 第{i}行 payload"));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let text = std::fs::read_to_string(current_file_path()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), threads * per_thread, "行数必须等于写入次数");

        // 每行都是完整记录：以 [INFO] 标记出现且都能被唯一 payload 匹配
        for t in 0..threads {
            for i in 0..per_thread {
                let needle = format!("线程{t} 第{i}行 payload");
                assert_eq!(
                    lines.iter().filter(|l| l.ends_with(&needle)).count(),
                    1,
                    "缺行或重复：{needle}"
                );
            }
        }
        assert!(lines.iter().all(|l| l.contains("[INFO] ")));
        assert!(lines.iter().all(|l| l.len() >= 23), "不得出现截断的半行");
    }

    /// 滚动策略：按天命名 + 7 天前的文件被清理，界内文件保留，非本应用文件不动。
    #[test]
    fn prune_removes_only_expired_app_logs() {
        let iso = Isolated::new("prune");
        let dir = directory_path();
        std::fs::create_dir_all(&dir).unwrap();

        let now = now_seconds();
        let fresh = dir.join("glacc-auto-20991231.log");
        let stale = dir.join("glacc-auto-20200101.log");
        let other = dir.join("not-ours.log");
        std::fs::write(&fresh, b"fresh").unwrap();
        std::fs::write(&stale, b"stale").unwrap();
        std::fs::write(&other, b"other").unwrap();

        // 把 stale 的最后写入时间压到 8 天前（用文件时间 API）
        let stale_time = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs((now - (RETENTION_DAYS + 1) * 24 * 3600) as u64);
        set_mtime(&stale, stale_time);

        let removed = prune_at_seconds(now);
        assert_eq!(removed, 1, "只应删除 1 个过期日志");
        assert!(!stale.exists(), "8 天前的日志必须删除");
        assert!(fresh.exists(), "界内日志必须保留");
        assert!(other.exists(), "非本应用文件不得删除");

        // 目录不存在时不 panic、返回 0
        std::fs::remove_dir_all(iso.dir.join("logs")).unwrap();
        assert_eq!(prune_at_seconds(now), 0);
    }

    /// 设置文件修改时间（测试辅助）：用 OpenOptions + SetFileTime 不便，改用 std 的
    /// File::set_times（Rust 1.75+ 稳定）。
    fn set_mtime(path: &Path, when: std::time::SystemTime) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }
}
