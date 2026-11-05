// 应用设置，逐字移植自旧 Avalonia 版
// src\GlaccAuto.Core\Settings\AppSettings.cs。
//
// 落盘位置：<数据根目录>/settings.json（旧版为 %APPDATA%\glacc-auto\settings.json），
// 数据根目录复用 store::data_root()，因此 GLACC_AUTO_DATA 隔离对设置同样生效。
// 序列化形态：System.Text.Json 默认命名（PascalCase 属性名）+ WriteIndented，此处用 serde rename 对齐。
// 缺字段补默认值（serde default + Default 实现），文件损坏整体回退默认并落一条日志 —— 与旧版一致：
// 配置坏了只是回到默认值，绝不让程序起不来。

use serde::{Deserialize, Serialize};

use crate::glacc::diag;
use crate::glacc::store;

/// 领取间隔（秒）范围。
pub const INTERVAL_SEC_MIN: i32 = 1;
pub const INTERVAL_SEC_MAX: i32 = 3600;
/// 网络失败重试次数范围。
pub const RETRY_COUNT_MAX: i32 = 10;

/// 应用设置。字段名/默认值与旧版 AppSettings 逐项一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// 定时领取时刻（hh:mm）：计划任务不存在时作为回退值；任务存在时以任务的真实触发时间为准
    #[serde(rename = "ScheduledTime")]
    pub scheduled_time: String,
    /// Server 酱 SendKey（空 = 不推送）
    #[serde(rename = "ServerKey")]
    pub server_key: String,
    /// 领取间隔下限（秒）
    #[serde(rename = "IntervalMinSec")]
    pub interval_min_sec: i32,
    /// 领取间隔上限（秒）
    #[serde(rename = "IntervalMaxSec")]
    pub interval_max_sec: i32,
    /// 网络失败后的自动重试次数（0~10），0 = 失败立即中断；总尝试次数 = 1 + 此值
    #[serde(rename = "NetworkRetryCount")]
    pub network_retry_count: i32,
    /// system | light | dark
    #[serde(rename = "Theme")]
    pub theme: String,
}

// 步 8：ScalePercent 字段已删除（窗口改为固定 1000×650 且禁止拉伸，
// 界面缩放功能整体下线）。旧 settings.json 里残留的 ScalePercent 由 serde 忽略，不影响读取。

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            scheduled_time: "08:00".to_string(),
            server_key: String::new(),
            interval_min_sec: 30,
            interval_max_sec: 40,
            network_retry_count: 3,
            theme: "system".to_string(),
        }
    }
}

impl AppSettings {
    /// 落盘值可信化：配置文件可能被外部编辑或被旧版本写坏，读取时夹紧一次，
    /// 保存时也走同一套，保证读写一致。
    pub fn normalize(&mut self) {
        self.interval_min_sec = self.interval_min_sec.clamp(INTERVAL_SEC_MIN, INTERVAL_SEC_MAX);
        self.interval_max_sec = self
            .interval_max_sec
            .clamp(self.interval_min_sec, INTERVAL_SEC_MAX);
        self.network_retry_count = self.network_retry_count.clamp(0, RETRY_COUNT_MAX);
        if !matches!(self.theme.as_str(), "system" | "light" | "dark") {
            self.theme = "system".to_string();
        }
        if !is_valid_hhmm(&self.scheduled_time) {
            self.scheduled_time = "08:00".to_string();
        }
    }

    /// 配置文件路径：与凭据、日志同一数据根目录。
    pub fn path() -> std::path::PathBuf {
        store::data_root().join("settings.json")
    }

    /// 读取：文件缺失 → 全默认；内容无法解析 → 全默认并记一条告警；成功 → 先 Normalize。
    pub fn load() -> Self {
        let path = Self::path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                diag::warn(&format!("读取配置失败，本次使用默认值：{e}"));
                return Self::default();
            }
        };
        match Self::from_json(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                diag::warn(&format!(
                    "配置文件无法解析，本次使用默认值：{}（{e}）",
                    path.display()
                ));
                Self::default()
            }
        }
    }

    /// 从 JSON 文本解析（缺字段按默认值补齐；解析后立即 Normalize）。
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        let mut cfg: AppSettings = serde_json::from_str(text)?;
        cfg.normalize();
        Ok(cfg)
    }

    /// 序列化（先 Normalize，保证落盘值与运行时值同一套规则）。
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut cfg = self.clone();
        cfg.normalize();
        serde_json::to_string_pretty(&cfg)
    }

    /// 原子保存：临时文件 + 改名，避免崩溃/断电留下半截 JSON（旧版同一套做法）。
    /// 失败只记日志，不阻断业务。
    pub fn save(&self) {
        match self.to_json() {
            Ok(json) => {
                if let Err(e) = store::write_atomic(&Self::path(), &json) {
                    diag::warn(&format!("保存配置失败：{e}"));
                }
            }
            Err(e) => diag::warn(&format!("保存配置失败：{e}")),
        }
    }

    /// 领取间隔区间（秒），已按 Normalize 口径夹紧，供调度层直接取用。
    pub fn interval_secs(&self) -> (i32, i32) {
        let min = self.interval_min_sec.clamp(INTERVAL_SEC_MIN, INTERVAL_SEC_MAX);
        let max = self.interval_max_sec.clamp(min, INTERVAL_SEC_MAX);
        (min, max)
    }
}

/// hh:mm 校验（旧版 TimeSpan.TryParse 的宽口径近似：只接受 H:MM / HH:MM，0..=23 / 0..=59）。
/// 旧版用 TimeSpan.TryParse，会接受 "8:00"、"08:00"，也接受 "1.02:03" 这类跨度写法；
/// 这里只覆盖设置页实际可能写入的形态，无法解析的一律回退 08:00（更保守，不会写入怪值）。
fn is_valid_hhmm(s: &str) -> bool {
    let s = s.trim();
    let Some((h, m)) = s.split_once(':') else {
        return false;
    };
    let (Ok(hour), Ok(minute)) = (h.trim().parse::<u32>(), m.trim().parse::<u32>()) else {
        return false;
    };
    hour <= 23 && minute <= 59
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glacc::store::ENV_GUARD;
    use std::path::PathBuf;

    struct Isolated {
        dir: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Isolated {
        fn new(tag: &str) -> Isolated {
            let lock = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("glacc-tauri-step2c-settings-{tag}"));
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

    /// 默认值与旧版逐项一致。
    #[test]
    fn defaults_match_legacy() {
        let d = AppSettings::default();
        assert_eq!(d.scheduled_time, "08:00");
        assert_eq!(d.server_key, "");
        assert_eq!(d.interval_min_sec, 30);
        assert_eq!(d.interval_max_sec, 40);
        assert_eq!(d.network_retry_count, 3);
        assert_eq!(d.theme, "system");
    }

    /// 序列化形态：PascalCase 属性名，与旧版 System.Text.Json 产物一致。
    #[test]
    fn json_shape_matches_legacy() {
        let json = AppSettings::default().to_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 6, "字段数应为 6（步 8 起不再有 ScalePercent）");
        for key in [
            "ScheduledTime",
            "ServerKey",
            "IntervalMinSec",
            "IntervalMaxSec",
            "NetworkRetryCount",
            "Theme",
        ] {
            assert!(obj.contains_key(key), "缺字段：{key}");
        }
        assert_eq!(obj["ScheduledTime"], "08:00");
        assert!(!obj.contains_key("ScalePercent"), "ScalePercent 应已随缩放功能下线");
        // WriteIndented：与旧版一致带缩进换行
        assert!(json.contains('\n'));
    }

    /// 缺字段补默认：只给一个字段时，其余全部回落默认值。
    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let cfg = AppSettings::from_json(r#"{"ServerKey":"SCTabc123"}"#).unwrap();
        assert_eq!(cfg.server_key, "SCTabc123");
        assert_eq!(cfg.scheduled_time, "08:00");
        assert_eq!(cfg.interval_min_sec, 30);
        assert_eq!(cfg.interval_max_sec, 40);
        assert_eq!(cfg.network_retry_count, 3);
        assert_eq!(cfg.theme, "system");

        let empty = AppSettings::from_json("{}").unwrap();
        assert_eq!(empty, AppSettings::default());
    }

    /// 损坏 JSON 回退全默认；非法值被 Normalize 夹紧。
    #[test]
    fn corrupt_json_and_normalize() {
        assert!(AppSettings::from_json("{oops").is_err());
        assert!(AppSettings::from_json("null").is_err());

        let mut cfg = AppSettings::from_json(
            r#"{"IntervalMinSec":0,"IntervalMaxSec":99999,
                "NetworkRetryCount":99,"Theme":"neon","ScheduledTime":"25:99"}"#,
        )
        .unwrap();
        cfg.normalize();
        assert_eq!(cfg.interval_min_sec, INTERVAL_SEC_MIN);
        assert_eq!(cfg.interval_max_sec, INTERVAL_SEC_MAX);
        assert_eq!(cfg.network_retry_count, RETRY_COUNT_MAX);
        assert_eq!(cfg.theme, "system");
        assert_eq!(cfg.scheduled_time, "08:00");
    }

    /// 保存后重新读取一致（原子写 + 隔离目录）。
    #[test]
    fn save_then_load_roundtrip() {
        let iso = Isolated::new("roundtrip");
        let mut cfg = AppSettings {
            server_key: "SCT1234567890".to_string(),
            interval_min_sec: 12,
            interval_max_sec: 18,
            scheduled_time: "21:35".to_string(),
            ..AppSettings::default()
        };
        cfg.normalize();
        cfg.save();

        assert_eq!(AppSettings::path(), iso.dir.join("settings.json"));
        assert!(AppSettings::path().is_file(), "必须写到隔离目录内");

        let loaded = AppSettings::load();
        assert_eq!(loaded, cfg);
        assert_eq!(loaded.interval_secs(), (12, 18));
    }

    /// 文件缺失 → 默认；损坏文件 → 默认且不崩。
    #[test]
    fn missing_and_corrupt_files() {
        let _iso = Isolated::new("missing");
        assert_eq!(AppSettings::load(), AppSettings::default());

        std::fs::write(AppSettings::path(), b"{ not json").unwrap();
        assert_eq!(AppSettings::load(), AppSettings::default());
    }

    /// hh:mm 校验口径。
    #[test]
    fn hhmm_validation() {
        assert!(is_valid_hhmm("08:00"));
        assert!(is_valid_hhmm("8:5"));
        assert!(is_valid_hhmm("23:59"));
        assert!(!is_valid_hhmm("24:00"));
        assert!(!is_valid_hhmm("08:60"));
        assert!(!is_valid_hhmm("0800"));
        assert!(!is_valid_hhmm(""));
    }
}
