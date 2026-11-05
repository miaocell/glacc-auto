// 用户凭证与账号态，逐字移植自旧 Avalonia 版
// src\GlaccAuto.Core\Glacc\GlaccCredentials.cs。
// 多账号下每个账号一份文件，路径由 AccountStore 决定并写入 storage_path
// （旧版单文件 %APPDATA%\glacc-auto\credentials.json 已自动迁移）。
// 落盘内容 = 装机盐（非个人信息）与用户数据（手机号、账号 sub、token）；
// 设备标识与设备档案由装机盐与手机号派生、不落盘，随手机号清空一并失效。
// 官方常量见 constants。存储层基础能力（数据根目录、账号命名、原子写、过渡期日志）在 store.rs；
// 本文件只保留凭据模型、派生与落盘。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::glacc::device::{self, random_bytes, sha256, DeviceProfile};
use crate::glacc::store;

/// 字段名与旧版 System.Text.Json 序列化产物逐字一致（无命名策略，属性名原样 PascalCase）；
/// 容器级 default：旧文件缺字段时按默认值读取（与 C# 反序列化缺属性保持初始值的语义一致）。
/// 派生属性（DeviceId 等）在旧版标 [JsonIgnore]，此处不设字段、以方法提供，天然不落盘。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    /// 手机号（登录用，"12xxxxxxxxx" 格式）
    #[serde(rename = "Phone")]
    pub phone: String,

    /// 进行中短信登录的目标手机号（与 phone 同格式）。
    /// 发送验证码时写入，登录成功转正为 phone；登录流程之外不改动 phone，
    /// 改号登录期间已登录会话的显示与游戏域设备身份保持不变。
    /// 与 verification_id 一样落盘，应用重启后可续用。
    #[serde(rename = "PendingPhone")]
    pub pending_phone: String,

    /// 装机盐（32 位 hex）：首次运行时随机生成并持久化，仅参与设备标识派生，不含任何用户信息。
    /// 退出登录不重置，使同一手机号在本机重登时复现同一套设备标识。
    #[serde(rename = "InstallSalt")]
    pub install_salt: String,

    /// 用户 ID（登录响应 sub）
    #[serde(rename = "Sub")]
    pub sub: String,

    #[serde(rename = "AccessToken")]
    pub access_token: String,
    #[serde(rename = "RefreshToken")]
    pub refresh_token: String,
    /// access_token 获取时间（Unix 秒）
    #[serde(rename = "ObtainedAt")]
    pub obtained_at: i64,
    /// access_token 有效期（秒，默认 7200）
    #[serde(rename = "ExpiresIn")]
    pub expires_in: i32,

    // ── 进行中的短信登录状态（验证码 5 分钟有效，应用重启后可续用）──
    #[serde(rename = "VerificationId")]
    pub verification_id: String,
    /// verification_id 签发时间（Unix 秒）
    #[serde(rename = "VerificationIdAt")]
    pub verification_id_at: i64,

    /// 本实例的落盘路径。由 AccountStore 等价物赋值；未赋值时（异常路径）
    /// 回退到旧版单文件位置，保证不会静默丢失写入。不落盘（旧版 [JsonIgnore]）。
    #[serde(skip)]
    pub storage_path: String,

    /// true = 磁盘上已有凭据文件但本机读不出内容（损坏、被占用、字段不兼容等）。
    /// 置位后 save 拒绝写入：宁可当前会话不落盘，也不能把一份读不懂的旧凭据
    /// 覆盖成空白 —— 那会让用户"凭空丢号"，且设备标识随装机盐一起变化。不落盘。
    #[serde(skip)]
    pub block_persist: bool,
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            phone: String::new(),
            pending_phone: String::new(),
            install_salt: String::new(),
            sub: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
            obtained_at: 0,
            expires_in: 7200, // 与旧版字段初始值一致：缺该字段的旧文件读回后仍是 7200
            verification_id: String::new(),
            verification_id_at: 0,
            storage_path: String::new(),
            block_persist: false,
        }
    }
}

impl Credentials {
    #[inline]
    pub fn has_token(&self) -> bool {
        !self.access_token.is_empty()
    }

    #[inline]
    pub fn has_refresh_token(&self) -> bool {
        !self.refresh_token.is_empty()
    }

    /// JWT 到期时刻（Unix 秒）。
    #[inline]
    pub fn jwt_expires_at(&self) -> i64 {
        self.obtained_at + self.expires_in as i64
    }

    /// 当前登录流程的目标手机号：进行中的改号登录取 pending_phone，否则取已登录的 phone。
    pub fn login_phone(&self) -> &str {
        if !self.pending_phone.is_empty() {
            &self.pending_phone
        } else {
            &self.phone
        }
    }

    /// 登录域设备 ID（32 位 hex，captcha meta 与 auth 请求头）。
    /// 由装机盐与手机号派生：同机同号恒定、换号或换机必不同，且无法由手机号反推。
    pub fn device_id(&self) -> String {
        self.derive_hex("glacc-deviceid:", &Self::phone_digits(&self.phone))
    }

    /// 登录域设备 ID（短信登录流程用）：跟随 login_phone 派生，派生规则同 device_id。
    pub fn login_device_id(&self) -> String {
        self.derive_hex("glacc-deviceid:", &Self::phone_digits(self.login_phone()))
    }

    /// 游戏域设备标识（32 位 hex，peerid / x-device-id / x-guid），派生规则同 device_id。
    pub fn peer_id(&self) -> String {
        self.derive_hex("glacc-peerid:", &Self::phone_digits(&self.phone))
    }

    /// 设备档案（UA 与 TLS 指纹来源）：按手机号哈希从内置池确定性选档，同号恒定、异号分散。
    pub fn device(&self) -> DeviceProfile {
        Self::device_for(&Self::phone_digits(&self.phone))
    }

    /// 登录域设备档案（短信登录流程用）：跟随 login_phone 选档，规则同 device。
    pub fn login_device(&self) -> DeviceProfile {
        Self::device_for(&Self::phone_digits(self.login_phone()))
    }

    fn device_for(digits: &str) -> DeviceProfile {
        if digits.len() == 11 {
            device::select_for_phone(digits)
        } else {
            device::PROFILES[0]
        }
    }

    /// JWT 是否仍有效（留 60s 余量）
    pub fn is_jwt_valid(&self) -> bool {
        self.has_token() && Self::now_seconds() < self.jwt_expires_at() - 60
    }

    pub fn now_seconds() -> i64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(_) => 0,
        }
    }

    /// 原子落盘到本实例的 storage_path；失败只记日志，不阻断业务（与旧版同语义）。
    pub fn save(&self) {
        let path = if self.storage_path.is_empty() {
            store::legacy_file_path()
        } else {
            PathBuf::from(&self.storage_path)
        };
        if self.block_persist {
            store::log(&format!(
                "凭据文件本机读取失败，已禁止写入以免覆盖旧数据：{}",
                path.display()
            ));
            return;
        }
        let json = match serde_json::to_string_pretty(self) {
            Ok(j) => j,
            Err(e) => {
                store::log(&format!("保存凭据失败：{e}"));
                return;
            }
        };
        if let Err(e) = store::write_atomic(&path, &json) {
            store::log(&format!("保存凭据失败：{e}"));
        }
    }

    /// 退出登录：清空手机号、账号 ID、令牌与进行中的短信登录。
    /// 设备标识与设备档案由装机盐与手机号派生、不落盘，随手机号清空一并失效。
    /// 不含持久化，调用方按需再存盘。
    pub fn clear_session(&mut self) {
        self.phone = String::new();
        self.pending_phone = String::new();
        self.sub = String::new();
        self.access_token = String::new();
        self.refresh_token = String::new();
        self.obtained_at = 0;
        self.expires_in = 7200;
        self.verification_id = String::new();
        self.verification_id_at = 0;
    }

    /// 首次使用时生成装机盐（32 位 hex）并落盘。
    pub fn ensure_install_salt(&mut self) {
        if !self.install_salt.is_empty() {
            return;
        }
        self.install_salt = new_hex32();
        self.save();
    }

    /// 派生 32 位 hex 设备标识：材料 = 装机盐 + 手机号（尚无有效手机号时只用装机盐，
    /// 保证标识始终非空且同机恒定）；域前缀使 device_id 与 peer_id 互相独立。
    fn derive_hex(&self, domain: &str, digits: &str) -> String {
        let material = if digits.len() == 11 {
            format!("{}:{}", self.install_salt, digits)
        } else {
            self.install_salt.clone()
        };
        let hash = sha256(format!("{domain}{material}").as_bytes());
        hex_lower(&hash[..16])
    }

    /// 任意手机号写法 → 纯数字串（去非数字、剥 86 前缀）；非法长度返回原数字串。
    pub fn phone_digits(phone: &str) -> String {
        let mut digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.len() == 13 && digits.starts_with("86") {
            digits = digits[2..].to_string();
        }
        digits
    }
}

/// 16 字节小写 hex（设备标识取前 16 字节，即 32 位 hex）。
fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 装机盐：16 字节系统随机数 → 32 位小写 hex。
fn new_hex32() -> String {
    let mut buf = [0u8; 16];
    random_bytes(&mut buf);
    hex_lower(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const SALT: &str = "0123456789abcdef0123456789abcdef";

    fn cred_with(phone: &str) -> Credentials {
        Credentials {
            phone: phone.to_string(),
            install_salt: SALT.to_string(),
            ..Credentials::default()
        }
    }

    /// 派生属性：已知盐与号的 32 位 hex 须与旧版一致（期望值由旧版算法独立复算）。
    #[test]
    fn derive_hex_matches_legacy() {
        let c = cred_with("13800138000");
        assert_eq!(c.device_id(), "41ebb2836b78a01f85ec6df243094cf1");
        assert_eq!(c.peer_id(), "a2a1bbd825e36e1077aee5aca4d7eed8");
        // 无待登录号时 login_device_id 跟随 phone
        assert_eq!(c.login_device_id(), c.device_id());

        // 待登录号存在时：login_device_id 跟随 pending，device_id/peer_id 不动
        let c = Credentials {
            pending_phone: "13912345678".to_string(),
            ..cred_with("13800138000")
        };
        assert_eq!(c.login_phone(), "13912345678");
        assert_eq!(c.login_device_id(), "d805c891be05289f773c96680e966744");
        assert_eq!(c.device_id(), "41ebb2836b78a01f85ec6df243094cf1");
        assert_eq!(c.peer_id(), "a2a1bbd825e36e1077aee5aca4d7eed8");

        // 数字串不合法（不足 11 位）：只用盐派生，与旧版同
        let c = cred_with("1380013");
        assert_eq!(c.device_id(), "29be07c75ac3b3feba69ffd7ef2a2e67");
        // 域前缀互相独立：同材料下 device_id 与 peer_id 必不同
        assert_ne!(c.device_id(), c.peer_id());
    }

    /// 派生属性经 Device/LoginDevice 链路选档：须与旧版（按手机号哈希）一致。
    #[test]
    fn derived_device_selection_matches_legacy() {
        let c = cred_with("15212345678");
        assert_eq!(c.device().device_model, "23054RA19C");
        assert_eq!(device::identifier_for(&c.device()), "okhttp4_android_13");

        // 待登录号选档独立于已登录号
        let c = Credentials {
            pending_phone: "13800138000".to_string(),
            ..cred_with("15212345678")
        };
        assert_eq!(c.device().device_model, "23054RA19C");
        assert_eq!(c.login_device().device_model, "2210132C");
        assert_eq!(
            device::identifier_for(&c.login_device()),
            "okhttp4_android_12"
        );

        // 号不合法：落第 0 档
        let c = cred_with("");
        assert_eq!(c.device(), device::PROFILES[0]);
        assert_eq!(c.login_device(), device::PROFILES[0]);
    }

    /// 手机号归一：去非数字、剥 86 前缀；长度不合法原样返回数字串。
    #[test]
    fn phone_digits_matches_legacy() {
        assert_eq!(Credentials::phone_digits("+86 13800138000"), "13800138000");
        assert_eq!(Credentials::phone_digits("8613800138000"), "13800138000");
        assert_eq!(Credentials::phone_digits("138 0013 8000"), "13800138000");
        assert_eq!(Credentials::phone_digits("1380013"), "1380013");
        assert_eq!(Credentials::phone_digits(""), "");
    }

    /// JWT 有效性：留 60s 余量；无 token 恒无效。
    #[test]
    fn jwt_validity_matches_legacy() {
        let now = Credentials::now_seconds();
        let mut c = Credentials {
            access_token: "t".to_string(),
            obtained_at: now,
            expires_in: 7200,
            ..Credentials::default()
        };
        assert_eq!(c.jwt_expires_at(), now + 7200);
        assert!(c.is_jwt_valid());
        // 余量边界：剩 59 秒即视为无效，剩 61 秒有效
        c.obtained_at = now - 7200 + 61;
        assert!(c.is_jwt_valid());
        c.obtained_at = now - 7200 + 59;
        assert!(!c.is_jwt_valid());
        c.access_token.clear();
        assert!(!c.is_jwt_valid());
    }

    /// 十字段序列化：字段名与形态须与旧版一致；storage_path / block_persist 不出现。
    #[test]
    fn serialized_shape_matches_legacy() {
        let c = Credentials {
            phone: "13800138000".to_string(),
            pending_phone: "13912345678".to_string(),
            install_salt: SALT.to_string(),
            sub: "sub-1".to_string(),
            access_token: "at".to_string(),
            refresh_token: "rt".to_string(),
            obtained_at: 1786116529,
            expires_in: 7200,
            verification_id: "vid".to_string(),
            verification_id_at: 1786116500,
            storage_path: r"C:\somewhere\accounts\abc.json".to_string(),
            block_persist: true,
        };
        let v: Value = serde_json::to_value(&c).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "AccessToken",
                "ExpiresIn",
                "InstallSalt",
                "ObtainedAt",
                "PendingPhone",
                "Phone",
                "RefreshToken",
                "Sub",
                "VerificationId",
                "VerificationIdAt",
            ],
            "落盘字段须为旧版十字段"
        );
        assert!(!obj.contains_key("storage_path"));
        assert!(!obj.contains_key("StoragePath"));
        assert!(!obj.contains_key("block_persist"));
        assert!(!obj.contains_key("BlockPersist"));
        assert_eq!(obj["ObtainedAt"], 1786116529);
        assert_eq!(obj["ExpiresIn"], 7200);

        // 反序列化缺字段：按默认值（ExpiresIn 为 7200，与 C# 属性初始值一致）
        let back: Credentials = serde_json::from_str(r#"{"Phone":"13800138000"}"#).unwrap();
        assert_eq!(back.phone, "13800138000");
        assert_eq!(back.expires_in, 7200);
        assert!(!back.block_persist);
        assert_eq!(back.storage_path, "");

        // 未知字段与 storage_path 回读：忽略未知、跳过字段不被注入
        let back: Credentials = serde_json::from_str(
            r#"{"Phone":"13800138000","StoragePath":"X","BlockPersist":true,"FutureField":1}"#,
        )
        .unwrap();
        assert_eq!(back.storage_path, "");
        assert!(!back.block_persist);
    }

    /// Save 的原子落盘与兜底路径、ClearSession、EnsureInstallSalt。
    /// 环境用 GLACC_AUTO_DATA 隔离到临时目录，不碰真实数据目录。
    /// 持 store 的进程级环境变量锁，与同样改写 GLACC_AUTO_DATA 的用例串行。
    #[test]
    fn save_clear_and_salt_with_isolated_data_dir() {
        let _guard = crate::glacc::store::ENV_GUARD
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("glacc-tauri-step1c");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("GLACC_AUTO_DATA", &dir);

        // 未设 storage_path：走旧版单文件兜底路径
        let mut c = Credentials::default();
        c.ensure_install_salt(); // 首次生成盐并落盘
        assert_eq!(c.install_salt.len(), 32);
        assert!(c.install_salt.chars().all(|ch| ch.is_ascii_hexdigit()));
        let legacy = store::legacy_file_path();
        assert_eq!(legacy, dir.join("credentials.json"));
        let text = std::fs::read_to_string(&legacy).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 10);
        // 缩进与旧版 WriteIndented 一致：pretty 输出含换行缩进
        assert!(text.contains("\n  \"Phone\""), "应为 pretty JSON：{text}");
        // 盐已就位：再次调用不改值、不重写
        let salt_before = c.install_salt.clone();
        c.ensure_install_salt();
        assert_eq!(c.install_salt, salt_before);
        // 临时文件不残留
        assert!(!dir.join("credentials.json.tmp").exists());

        // 显式 storage_path：写目标文件；覆盖写不报错
        let target = dir.join("accounts").join("abc.json");
        c.storage_path = target.to_string_lossy().to_string();
        c.access_token = "at-1".to_string();
        c.save();
        c.access_token = "at-2".to_string();
        c.save();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(v["AccessToken"], "at-2");

        // BlockPersist：拒绝写入（磁盘内容保持原样）
        c.block_persist = true;
        c.access_token = "at-3".to_string();
        c.save();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(v["AccessToken"], "at-2");
        c.block_persist = false;

        // ClearSession：清空会话但保留安装盐与 storage_path
        c.clear_session();
        assert_eq!(c.phone, "");
        assert_eq!(c.pending_phone, "");
        assert_eq!(c.sub, "");
        assert_eq!(c.access_token, "");
        assert_eq!(c.refresh_token, "");
        assert_eq!(c.obtained_at, 0);
        assert_eq!(c.expires_in, 7200);
        assert_eq!(c.verification_id, "");
        assert_eq!(c.verification_id_at, 0);
        assert_eq!(c.install_salt, salt_before, "退出登录不重置装机盐");
        assert_eq!(c.storage_path, target.to_string_lossy());

        std::env::remove_var("GLACC_AUTO_DATA");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
