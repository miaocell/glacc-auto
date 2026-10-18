// 多账号存储与导入导出，逐字移植自旧 Avalonia 版
// src\GlaccAuto.Core\Glacc\AccountStore.cs（AccountStore）与 src\GlaccAuto.Core\AppData.cs，
// 并按 miao 2026-10-17 步 1c-b 任务书新增导出/导入。
//
// 每个账号一份凭据文件，按手机号哈希命名（文件名不含手机号，避免 PII 泄漏）。
// 目录 %APPDATA%\glacc-auto\accounts\{key}.json；旧版单文件凭据在首次使用时自动迁移。
// 账号"是否算已登录"以文件内 Phone 非空为准：只发了验证码但未登录成功的半成品文件
// 留在目录里供同号复用，但不出现在账号列表中。
//
// 上限拦截说明：旧版两道拦截在 ViewModel（发码前 MainWindowViewModel.cs:1104、登录落盘前
// :1167）；本模块只提供 can_add 判定与 list_accounts 截断，UI 与登录链的接线在后续步骤。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::glacc::credentials::Credentials;
use crate::glacc::device::sha256;

/// 测试用环境变量互斥锁：凡修改 GLACC_AUTO_DATA 的用例（本模块与 credentials.rs）须共同持有，
/// 避免 cargo test 并行线程互相踩踏同一进程的环境变量。
#[cfg(test)]
pub(crate) static ENV_GUARD: Mutex<()> = Mutex::new(());

// ── 数据根目录（原 AppData.cs）──

/// 应用数据根目录的唯一来源：<c>%APPDATA%\glacc-auto</c>。
/// 环境变量 <c>GLACC_AUTO_DATA</c> 非空时改用其值——供自动化测试与便携实例把读写隔离到别处。
/// 差异说明：旧版走 SpecialFolder.ApplicationData（注册表），此处读进程环境变量 APPDATA；
/// 常规桌面进程两者一致，仅显式改 %APPDATA% 的怪场景有别（旧版注释亦说明该文件夹不受进程环境变量影响）。
/// 未设置该变量时行为与旧版完全一致。
pub fn data_root() -> PathBuf {
    match std::env::var("GLACC_AUTO_DATA") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let appdata = std::env::var("APPDATA").unwrap_or_default();
            Path::new(&appdata).join("glacc-auto")
        }
    }
}

/// 账号目录（不存在时由写入方按需创建）。
pub fn directory_path() -> PathBuf {
    data_root().join("accounts")
}

/// 旧版单账号凭据文件路径，仅用于一次性迁移与未绑定账号时的兜底落盘。
pub fn legacy_file_path() -> PathBuf {
    data_root().join("credentials.json")
}

// ── 存储基础能力（credentials.rs 复用）──

/// 临时文件 + 原子替换：多线程并发保存或进程中途退出都不会留下写了一半的凭证文件。
/// 与旧版一致：先建目录，写 path.tmp，再原子改名覆盖目标。
pub(crate) fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir)?;
        }
    }
    // 串行化并发保存（对应旧版 static SaveGate）
    static SAVE_GATE: Mutex<()> = Mutex::new(());
    let _guard = SAVE_GATE.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    fs::write(&tmp, content)?;
    // std::fs::rename 在 Windows 上走 MoveFileEx(REPLACE_EXISTING)，等价 C# File.Move(..., overwrite: true)
    fs::rename(&tmp, path)
}

/// 过渡期日志入口：diag 模块落地后转正式文件日志（按天滚动、7 天清理）。
/// 保留本函数是为了让各业务模块不必关心日志实现，调用点零改动。
pub(crate) fn log(msg: &str) {
    crate::glacc::diag::warn(msg);
}

// ── 命名与上限（AccountStore）──

/// 无手机号时的占位键（尚未登录的新账号槽位）。
pub const NO_KEY: &str = "";

/// 家庭自用账号硬上限。超出此数的账号一律不列出、不登录、不领取：
/// 本项目定位是自家几台设备自用，不做多账号池，也不做并发。
/// 上限判定集中在存储层（count / can_add），界面与调度只问结果，避免各处各写一份数字。
pub const MAX_ACCOUNTS: usize = 3;

/// 手机号数字串 → 账号键（16 位 hex）。空串表示无账号。
/// 算法与旧版逐字一致：SHA256("glacc-account:" + 数字串) 取前 8 字节转小写 hex。
pub fn key_for(phone_digits: &str) -> String {
    let digits = Credentials::phone_digits(phone_digits);
    if digits.chars().count() != 11 {
        return NO_KEY.to_string();
    }
    let hash = sha256(format!("glacc-account:{digits}").as_bytes());
    hex_lower(&hash[..8])
}

/// 账号键 → 凭据文件路径。
pub fn path_for(key: &str) -> PathBuf {
    directory_path().join(if key.is_empty() {
        "_pending.json".to_string()
    } else {
        format!("{key}.json")
    })
}

/// 已登录账号摘要（键 + 手机号数字串 + 最近使用时间），按最近使用倒序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub key: String,
    pub phone_digits: String,
    /// 最近使用时间（Unix 秒；旧版为文件最后写入时间的 UTC 时间点）
    pub last_used: i64,
}

/// 已登录账号列表（只统计能通过手机号校验的有效凭据文件），按最近使用倒序，截断到上限。
pub fn list_accounts() -> Vec<Account> {
    let mut list: Vec<Account> = Vec::new();
    let dir = directory_path();
    // 枚举失败只记日志（对应旧版 catch：目录不存在或不可读时返回空表）
    let entries = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log(&format!("枚举账号目录失败：{e}"));
            }
            return list;
        }
    };
    for entry in entries.flatten() {
        let path = match entry.file_type() {
            Ok(t) if t.is_file() => entry.path(),
            _ => continue,
        };
        if !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        let cred = match try_read(&path) {
            Some(c) => c,
            None => continue,
        };
        if cred.phone.is_empty() {
            continue;
        }
        let digits = Credentials::phone_digits(&cred.phone);
        if digits.chars().count() != 11 {
            continue;
        }
        let key = key_for(&digits);
        // 账号一律以"手机号哈希"命名：文件名与内容对不上（被复制改名、手工放错文件）时
        // 按 key 会读到另一个路径，出现"列表里有号、一点就显示未登录"的幽灵条目。
        // 这里直接跳过并留痕，宁可少列一个号，也不给出点了没反应的入口。
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !stem.eq_ignore_ascii_case(&key) {
            log(&format!(
                "跳过账号文件名与手机号不符的文件：{}",
                path.file_name().and_then(|s| s.to_str()).unwrap_or("")
            ));
            continue;
        }
        list.push(Account {
            key,
            phone_digits: digits,
            last_used: modified_seconds(&path),
        });
    }
    // 硬上限：按最近使用倒序只保留前 MaxAccounts 个。目录里多出来的文件（换机拷贝、
    // 旧版本遗留）不删除，只是不参与列表与领取，避免出现"界面看不到但后台在跑"。
    list.sort_by(|a, b| b.last_used.cmp(&a.last_used).then(b.key.cmp(&a.key)));
    list.truncate(MAX_ACCOUNTS);
    list
}

/// 当前已登录账号数（只统计能通过手机号校验的有效凭据文件）。
pub fn count() -> usize {
    list_accounts().len()
}

/// 是否还能新增账号。phone_digits 已在列表里时返回 true：
/// 同一个号重新登录属于"更新"，不该被上限挡住。
pub fn can_add(phone_digits: &str) -> bool {
    let accounts = list_accounts();
    if accounts.len() < MAX_ACCOUNTS {
        return true;
    }
    let digits = Credentials::phone_digits(phone_digits);
    digits.chars().count() == 11 && accounts.iter().any(|a| a.phone_digits == digits)
}

// ── 读取 / 落位 / 删除（AccountStore）──

/// 读取指定账号凭据。文件缺失 → 全新凭据（立即补齐装机盐，可正常落盘）；
/// 文件存在但读不出 → 空凭据并置 block_persist，
/// 阻止本次会话把读不懂的旧凭据覆盖成空白（防止"凭空丢号"）。
pub fn load(key: &str) -> Credentials {
    let path = path_for(key);
    if let Some(mut loaded) = try_read(&path) {
        loaded.storage_path = path.to_string_lossy().to_string();
        loaded.ensure_install_salt();
        return loaded;
    }
    let mut fresh = Credentials {
        storage_path: path.to_string_lossy().to_string(),
        block_persist: path.exists(),
        ..Credentials::default()
    };
    // 新账号也要当场拿到装机盐：设备标识由盐派生，缺盐会让首次登录的设备指纹每次都变。
    // block_persist 时 save 内部拒写，该盐只活在本会话（旧文件本就读不出，无从复现旧标识）。
    fresh.ensure_install_salt();
    fresh
}

/// 登录成功后把凭据归位到"手机号哈希"命名的文件。
/// 登录往往发生在未命名槽位（_pending.json）或旧键上：不归位的话，下次按手机号去找
/// 会命中不存在的文件，刚登录好的号会被当成"未登录"。
/// 返回归位后的账号键；手机号无效时返回 NO_KEY。
pub fn adopt_phone_key(cred: &mut Credentials) -> String {
    let digits = Credentials::phone_digits(&cred.phone);
    if digits.chars().count() != 11 {
        return NO_KEY.to_string();
    }
    let key = key_for(&digits);
    let new_path = path_for(&key);
    let old_path = cred.storage_path.clone();

    // 同号重登：目标文件已存在时先保住旧装机盐，否则设备标识会被换掉（等于换了一台设备）
    preserve_existing_salt(cred, &new_path);
    cred.storage_path = new_path.to_string_lossy().to_string();

    if old_path.eq_ignore_ascii_case(&cred.storage_path) {
        cred.save();
        return key;
    }

    let _ = fs::create_dir_all(directory_path());
    cred.save();
    // 同一份凭据已改名落盘，旧槽位里的半成品不该长期留着
    if !old_path.is_empty() {
        if let Err(e) = fs::remove_file(&old_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                log(&format!("清理旧凭据槽位失败：{e}"));
            }
        }
    }
    key
}

/// 删除账号凭据文件；失败只记日志（文件被占用时不阻断 UI）。
pub fn remove(key: &str) {
    let path = path_for(key);
    if let Err(e) = fs::remove_file(&path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            log(&format!("删除账号凭据失败：{e}"));
        }
    }
}

/// 把某账号标记为"最近使用"（当前激活账号 = 最近使用的账号）。
/// 只刷新凭据文件的修改时间，内容零改动（原子替换，中途退出也不会写坏凭据）。
/// 供"切换账号"使用，不参与领取判断。
pub fn touch(key: &str) -> Result<(), String> {
    let path = path_for(key);
    let text = fs::read_to_string(&path).map_err(|e| format!("读取凭据失败：{e}"))?;
    write_atomic(&path, &text).map_err(|e| format!("更新账号时间失败：{e}"))
}

/// 首次运行迁移：旧版 <c>credentials.json</c> 里有已登录账号时，搬进账号目录，
/// 并把原文件改名备份（不直接删除，迁移出问题时人工可恢复）。
/// 返回迁移后的账号键；无需迁移或失败时返回 NO_KEY。
pub fn migrate_legacy_if_needed() -> String {
    let legacy = legacy_file_path();
    if !legacy.exists() {
        return NO_KEY.to_string();
    }
    let legacy_cred = match fs::read_to_string(&legacy)
        .ok()
        .and_then(|t| serde_json::from_str::<Credentials>(&t).ok())
    {
        Some(c) => c,
        None => return NO_KEY.to_string(),
    };
    let digits = Credentials::phone_digits(&legacy_cred.phone);
    if digits.chars().count() != 11 {
        return NO_KEY.to_string();
    }
    let key = key_for(&digits);
    let path = path_for(&key);
    if !path.exists() {
        if let Err(e) = fs::create_dir_all(directory_path()).and_then(|_| fs::copy(&legacy, &path))
        {
            log(&format!("迁移旧凭据失败：{e}"));
            return NO_KEY.to_string();
        }
        log("已迁移旧版单账号凭据到新账号目录");
    }
    let backup = PathBuf::from(format!("{}.bak", legacy.display()));
    if backup.exists() {
        if let Err(e) = fs::remove_file(&backup) {
            log(&format!("迁移旧凭据失败：{e}"));
            return NO_KEY.to_string();
        }
    }
    if let Err(e) = fs::rename(&legacy, &backup) {
        log(&format!("迁移旧凭据失败：{e}"));
        return NO_KEY.to_string();
    }
    key
}

/// 同号重新登录时保住旧装机盐：设备标识由装机盐派生，被新盐覆盖等于换了一台设备，
/// 服务端侧会呈现为全新设备（风控面）。只在目标文件已有合法盐时才沿用。
fn preserve_existing_salt(cred: &mut Credentials, path: &Path) {
    if let Some(existing) = try_read(path) {
        let salt = &existing.install_salt;
        if salt.chars().count() == 32 && cred.install_salt != *salt {
            cred.install_salt = salt.clone();
        }
    }
}

fn try_read(path: &Path) -> Option<Credentials> {
    if !path.exists() {
        return None;
    }
    match fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Credentials>(&t).ok())
    {
        Some(c) => Some(c),
        None => {
            log(&format!(
                "读取凭据失败（{}）",
                path.file_name().and_then(|s| s.to_str()).unwrap_or("")
            ));
            None
        }
    }
}

/// 文件最后写入时间（Unix 秒）。旧版用 File.GetLastWriteTimeUtc 排序"最近使用"，语义一致。
fn modified_seconds(path: &Path) -> i64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ── 导出 / 导入（步 1c-b 新增；旧版无此功能）──
//
// 安全提醒：导出文件内含明文手机号与 access/refresh token（相当于是账号的"钥匙"，
// 拿到即代表对应账号被完整接管）。旧版凭据本就是明文 JSON，导出不降低本机安全性；
// 但导出文件可能被拷到 U 盘或云端，属含密钥文件，落盘或外传前须由使用方自行评估。
// 是否加密（如 Windows DPAPI）等 miao 拍板；本次按任务书只做 JSON。

const EXPORT_VERSION: u32 = 1;

/// 导出外层信封。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportEnvelope {
    /// 导出格式版本（当前 1）
    pub version: u32,
    /// 导出时刻（Unix 秒）
    #[serde(rename = "exportedAt")]
    pub exported_at: i64,
    /// 账号凭据（与存储层同格式；复用 Credentials 序列化，字段名即旧版落盘字段名）
    pub accounts: Vec<Credentials>,
}

/// 导出错误。
#[derive(Debug, PartialEq, Eq)]
pub enum ExportError {
    /// 没有可导出的账号
    NoAccounts,
    /// 文件写入失败
    Io(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::NoAccounts => write!(f, "没有可导出的账号"),
            ExportError::Io(msg) => write!(f, "导出失败：{msg}"),
        }
    }
}

/// 导出全部已登录账号到指定文件（外到内：信封 → 账号列表 → 凭据，与实际读盘同源）。
/// 只导出出现在账号列表里的有效账号；磁盘上未列入的遗留文件不导出（与列表口径一致）。
pub fn export_accounts(path: &Path) -> Result<ExportEnvelope, ExportError> {
    let accounts = list_accounts();
    if accounts.is_empty() {
        return Err(ExportError::NoAccounts);
    }
    // 与真实读盘同源：逐个 load，得到的就是当前生效的凭据（含各自 storage_path 归属）
    let creds: Vec<Credentials> = accounts.iter().map(|a| load(&a.key)).collect();
    let envelope = ExportEnvelope {
        version: EXPORT_VERSION,
        exported_at: Credentials::now_seconds(),
        accounts: creds,
    };
    let json = serde_json::to_string_pretty(&envelope)
        .map_err(|e| ExportError::Io(format!("序列化导出内容失败：{e}")))?;
    write_atomic(path, &json).map_err(|e| ExportError::Io(e.to_string()))?;
    Ok(envelope)
}

/// 导入结果摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    /// 成功导入（落位）的账号键，按文件内顺序
    pub imported: Vec<String>,
    /// 被跳过的条目及原因（号码非法 / 超上限 / 同号已在列表中）
    pub skipped: Vec<(String, String)>,
}

/// 导入错误。
#[derive(Debug, PartialEq, Eq)]
pub enum ImportError {
    /// 文件读不到或不是合法 JSON
    Read(String),
    /// 信封版本不支持
    UnsupportedVersion(u32),
    /// 文件里没有账号条目
    NoAccounts,
    /// 全部条目都被跳过（号码非法等原因）
    AllSkipped,
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::Read(msg) => write!(f, "读取导入文件失败：{msg}"),
            ImportError::UnsupportedVersion(v) => write!(f, "不支持的导出文件版本：{v}"),
            ImportError::NoAccounts => write!(f, "导入文件里没有账号"),
            ImportError::AllSkipped => write!(f, "所有账号都被跳过（号码非法或超上限）"),
        }
    }
}

/// 从导出文件导入账号。
/// 盐冲突策略 = 本机优先（与旧版 PreserveExistingSalt 同语义）：
/// 同号已在本机存在时，沿用本机旧盐——设备标识保持不变，导入件的盐被忽略；
/// 本机同号不存在时，沿用导入件自带盐（同时插入多台机器拷来的凭据，各账号仍各自恒等）。
/// 上限 3：按已导入数实时判定，超出的条目跳过并记原因，不整单失败。
pub fn import_accounts(path: &Path) -> Result<ImportSummary, ImportError> {
    let text = fs::read_to_string(path).map_err(|e| ImportError::Read(e.to_string()))?;
    let envelope: ExportEnvelope =
        serde_json::from_str(&text).map_err(|e| ImportError::Read(e.to_string()))?;
    if envelope.version != EXPORT_VERSION {
        return Err(ImportError::UnsupportedVersion(envelope.version));
    }
    if envelope.accounts.is_empty() {
        return Err(ImportError::NoAccounts);
    }

    let mut summary = ImportSummary {
        imported: Vec::new(),
        skipped: Vec::new(),
    };
    for cred in envelope.accounts {
        let digits = Credentials::phone_digits(&cred.phone);
        if digits.chars().count() != 11 {
            summary.skipped.push((digits, "手机号无效".to_string()));
            continue;
        }
        // 上限：can_add 里"已在列表的号"视为更新、不占名额，与旧版口径一致
        if !can_add(&digits) {
            summary
                .skipped
                .push((digits, format!("已达上限 {MAX_ACCOUNTS} 个账号")));
            continue;
        }
        let mut incoming = cred;
        // 导入不携带存储层私有状态
        incoming.storage_path = String::new();
        incoming.block_persist = false;
        let key = adopt_phone_key(&mut incoming);
        if key.is_empty() {
            summary.skipped.push((digits, "手机号无效".to_string()));
        } else {
            summary.imported.push(key);
        }
    }

    if summary.imported.is_empty() {
        return Err(ImportError::AllSkipped);
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const SALT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SALT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// 测试隔离守卫：GLACC_AUTO_DATA 指向唯一临时目录。
    /// 守卫 Drop 时清理并把变量改回进程原有值；无现成路径时回退进程级守卫，
    /// 与同进程其他写盘用例串行。
    struct Isolated {
        dir: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Isolated {
        /// 构造即独占：先取进程级锁，再设环境变量并建目录，保证与所有环境变量用例串行。
        /// 同一用例需要第二个隔离环境时，必须先把前一个 drop 掉再新建。
        fn new(tag: &str) -> Isolated {
            let lock = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("glacc-tauri-step1cb-{tag}"));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            std::env::set_var("GLACC_AUTO_DATA", &dir);
            Isolated { dir, _lock: lock }
        }
    }

    impl Drop for Isolated {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
            std::env::remove_var("GLACC_AUTO_DATA");
        }
    }

    fn write_cred(phone: &str, salt: &str, access: &str) -> String {
        let mut c = Credentials {
            phone: phone.to_string(),
            install_salt: salt.to_string(),
            access_token: access.to_string(),
            refresh_token: "rt".to_string(),
            sub: "sub".to_string(),
            ..Credentials::default()
        };
        let key = adopt_phone_key(&mut c);
        assert!(!key.is_empty());
        key
    }

    /// 显式设置文件修改时间为 now - secs_ago（列表按 mtime 排序，同秒写入会并列，需确定性）。
    fn set_mtime(path: &Path, secs_ago: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs_ago);
        let f = fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
    }

    /// touch = 把目标账号顶到"最近使用"第一位（当前激活账号），且凭据内容零改动。
    #[test]
    fn touch_bumps_account_to_front() {
        let iso = Isolated::new("touch");
        let key_a = write_cred("13800138001", "salt-a", "at-a");
        let key_b = write_cred("13800138002", "salt-b", "at-b");
        // 同秒写入会并列，用 set_mtime 定序：B 比 A 新 → B 是当前账号
        set_mtime(&path_for(&key_a), 200);
        set_mtime(&path_for(&key_b), 100);
        assert_eq!(list_accounts()[0].key, key_b);

        touch(&key_a).expect("touch should succeed");
        let accounts = list_accounts();
        assert_eq!(accounts[0].key, key_a);
        assert_eq!(accounts[0].phone_digits, "13800138001");

        // 内容零改动
        let cred = load(&key_a);
        assert_eq!(cred.phone, "13800138001");
        assert_eq!(cred.access_token, "at-a");
        drop(iso);
    }

    /// 命名规则：SHA256("glacc-account:"+数字串) 前 8 字节 hex（期望值由旧版算法独立复算）。
    #[test]
    fn key_for_matches_legacy() {
        assert_eq!(key_for("13800138000"), "d4c956e093be53ba");        assert_eq!(key_for("13912345678"), "8c19db1896c64836");
        assert_eq!(key_for("15212345678"), "5d5bbac7a7acec68");
        assert_eq!(key_for("18612345678"), "0e1c950b867e78cd");
        // 带 86 前缀/空号：归一后判定
        assert_eq!(key_for("8613800138000"), "d4c956e093be53ba");
        assert_eq!(key_for(""), NO_KEY);
        assert_eq!(key_for("1380013"), NO_KEY);
    }

    /// 路径规则：未命名槽位用 _pending.json，账号用 {key}.json。
    #[test]
    fn path_rules() {
        let d = Isolated::new("paths");
        assert_eq!(directory_path(), d.dir.join("accounts"));
        assert_eq!(legacy_file_path(), d.dir.join("credentials.json"));
        assert!(path_for(NO_KEY).ends_with("_pending.json"));
        assert!(path_for("d4c956e093be53ba").ends_with("d4c956e093be53ba.json"));
    }

    /// 隔离落盘 + 上限 3 + 列表口径：三次落盘可列出；第四个号超限；
    /// 已列出的号重登不算新增；文件名与手机号不符的文件被跳过。
    #[test]
    fn isolation_cap_and_listing() {
        let _iso = Isolated::new("cap");

        let k1 = write_cred("13800138000", SALT_A, "at1");
        let k2 = write_cred("13912345678", SALT_A, "at2");
        assert_eq!(count(), 2);
        // 半成品槽位（无手机号）不占名额、不进列表
        let mut pending = Credentials {
            install_salt: SALT_A.to_string(),
            ..Credentials::default()
        };
        pending.storage_path = path_for(NO_KEY).to_string_lossy().to_string();
        pending.save();
        assert_eq!(count(), 2, "只发了码未登录的号不应算已用名额");
        assert!(can_add("18612345678"), "未满 3 个时新号可加");

        let k3 = write_cred("15212345678", SALT_A, "at3");
        assert_eq!(k1, "d4c956e093be53ba");
        assert_eq!(count(), 3);
        assert!(can_add("13800138000"), "同号重登属于更新，不受上限影响");

        // 第四号：文件已满，新号应被上限拒绝
        assert!(!can_add("13000000001"), "第 4 个号应被上限拒绝");
        assert_eq!(count(), 3);

        // 列表按最近使用倒序：显式分层 mtime 后，最新写入的 k3 居首
        set_mtime(&path_for(&k1), 300);
        set_mtime(&path_for(&k2), 200);
        set_mtime(&path_for(&k3), 100);
        let list = list_accounts();
        assert_eq!(list[0].key, k3);
        assert_eq!(list[1].key, k2);
        assert_eq!(list[2].key, k1);
        assert!(list.iter().any(|a| a.phone_digits == "13800138000"));

        // 幽灵条目：文件名与内容不符的文件不进列表
        let ghost = directory_path().join("ffffffffffffffff.json");
        let mut ghost_cred = Credentials {
            phone: "13000000008".to_string(),
            install_salt: SALT_A.to_string(),
            ..Credentials::default()
        };
        ghost_cred.storage_path = ghost.to_string_lossy().to_string();
        ghost_cred.save();
        let list = list_accounts();
        assert!(!list.iter().any(|a| a.phone_digits == "13000000008"));
        assert_eq!(count(), 3);

        // 删除账号
        remove(&k2);
        assert_eq!(count(), 2);
        assert!(can_add("13000000001"), "删除后腾出名额");
    }

    /// 盐保留：同号重登（含 pending 槽位归位）时沿用本机旧盐；本机不存在时沿用导入件盐。
    #[test]
    fn salt_preserved_local_first() {
        let _iso = Isolated::new("salt");

        let key = write_cred("13800138000", SALT_A, "at1");
        // 同号带新盐 A→B 重登：目标文件已有合法盐，须沿用旧盐 A
        let mut again = Credentials {
            phone: "13800138000".to_string(),
            install_salt: SALT_B.to_string(),
            access_token: "at2".to_string(),
            ..Credentials::default()
        };
        let key2 = adopt_phone_key(&mut again);
        assert_eq!(key2, key);
        assert_eq!(again.install_salt, SALT_A, "同号重登须保留本机旧盐");
        let reloaded = load(&key);
        assert_eq!(reloaded.install_salt, SALT_A);

        // 新号（本机不存在）：沿用自带盐
        let mut fresh = Credentials {
            phone: "13000000001".to_string(),
            install_salt: SALT_B.to_string(),
            ..Credentials::default()
        };
        adopt_phone_key(&mut fresh);
        assert_eq!(fresh.install_salt, SALT_B);
        assert_eq!(load(&key_for("13000000001")).install_salt, SALT_B);
    }

    /// BlockPersist：磁盘上已有读不懂的凭据时，load 返回空凭据且拒绝覆盖。
    #[test]
    fn block_persist_on_unreadable_file() {
        let _iso = Isolated::new("block");

        let key = "d4c956e093be53ba";
        let path = path_for(key);
        fs::create_dir_all(directory_path()).unwrap();
        fs::write(&path, "{ this is not json").unwrap();

        let mut cred = load(key);
        assert!(cred.block_persist, "读不懂的旧文件应阻止落盘");
        assert_eq!(cred.phone, "");
        assert!(!cred.install_salt.is_empty(), "本会话仍应有随机盐可用");
        cred.access_token = "should-not-land".to_string();
        cred.save();
        // 磁盘内容保持原样（未被覆盖成空白）
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ this is not json");
        assert_eq!(count(), 0, "读不懂的文件不进列表");
    }

    /// 旧 credentials.json 迁移：有效账号迁入账号目录并留 .bak；重复调用幂等；
    /// 无手机号/损坏文件不迁移。
    #[test]
    fn legacy_migration() {
        let _iso = Isolated::new("migrate");

        // 无效的旧文件：不迁移、原文件保持
        let legacy = legacy_file_path();
        fs::create_dir_all(data_root()).unwrap();
        fs::write(
            &legacy,
            serde_json::to_string(&Credentials {
                phone: "1380013".to_string(),
                ..Credentials::default()
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(migrate_legacy_if_needed(), NO_KEY);
        assert!(legacy.exists());

        // 有效账号：迁入账号目录，原文件改名 .bak
        fs::write(
            &legacy,
            serde_json::to_string(&Credentials {
                phone: "13800138000".to_string(),
                install_salt: SALT_A.to_string(),
                access_token: "legacy-at".to_string(),
                ..Credentials::default()
            })
            .unwrap(),
        )
        .unwrap();
        let key = migrate_legacy_if_needed();
        assert_eq!(key, "d4c956e093be53ba");
        assert!(!legacy.exists(), "原文件应改名（不删除）");
        assert!(
            data_root().join("credentials.json.bak").exists(),
            "须留 .bak 备份"
        );
        let migrated = load(&key);
        assert_eq!(migrated.access_token, "legacy-at");
        assert_eq!(count(), 1);

        // 幂等：再次调用无事发生
        assert_eq!(migrate_legacy_if_needed(), NO_KEY);

        // 已有同名账号文件时：不覆盖，仅改名备份并返回该键
        fs::write(
            &legacy,
            serde_json::to_string(&Credentials {
                phone: "13800138000".to_string(),
                access_token: "newer-at".to_string(),
                ..Credentials::default()
            })
            .unwrap(),
        )
        .unwrap();
        let key = migrate_legacy_if_needed();
        assert_eq!(key, "d4c956e093be53ba");
        assert!(data_root().join("credentials.json.bak").exists());
        assert_eq!(
            load(&key).access_token,
            "legacy-at",
            "已存在的账号文件不应被覆盖"
        );
    }

    /// 导出：无账号报错；有账号时信封含 version/exportedAt/accounts 与十字段凭据。
    #[test]
    fn export_envelope_shape() {
        let iso = Isolated::new("export");
        // 空目录导出 → NoAccounts
        assert!(matches!(
            export_accounts(&iso.dir.join("out.json")),
            Err(ExportError::NoAccounts)
        ));

        write_cred("13800138000", SALT_A, "at1");
        write_cred("13912345678", SALT_A, "at2");
        let out = iso.dir.join("export.json");
        let env = export_accounts(&out).unwrap();
        assert_eq!(env.version, 1);
        assert!(env.exported_at > 0);
        assert_eq!(env.accounts.len(), 2);
        let v: Value = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["accounts", "exportedAt", "version"]);
        assert_eq!(obj["version"], 1);
        // 账号条目仍为十字段落盘形态（不含 storage_path/block_persist）
        let one = obj["accounts"][0].as_object().unwrap();
        assert_eq!(one.len(), 10);
        assert!(!one.contains_key("storage_path"));
        assert_eq!(one["InstallSalt"], SALT_A);
    }

    /// 导出后再导入（换目录）：数据完整；导入不携带存储层私有字段。
    #[test]
    fn export_then_import_roundtrip() {
        let src = Isolated::new("rt-src");
        write_cred("13800138000", SALT_A, "at-1");
        write_cred("13912345678", SALT_B, "at-2");
        let out = src.dir.join("export.json");
        export_accounts(&out).unwrap();
        let bytes = fs::read(&out).unwrap();
        drop(src); // 独立导出文件，脱离源环境

        // 目标环境：全新数据目录
        let dst = Isolated::new("rt-dst");
        let out2 = dst.dir.join("in.json");
        fs::write(&out2, &bytes).unwrap();
        let summary = import_accounts(&out2).unwrap();
        assert_eq!(summary.imported.len(), 2);
        assert!(summary.skipped.is_empty());
        assert_eq!(count(), 2);

        // 数据完整：手机号、盐、token 全部一致
        let a = load(&key_for("13800138000"));
        assert_eq!(a.phone, "13800138000");
        assert_eq!(a.install_salt, SALT_A);
        assert_eq!(a.access_token, "at-1");
        assert_eq!(a.refresh_token, "rt");
        assert_eq!(a.sub, "sub");
        assert_eq!(
            a.storage_path,
            path_for(&key_for("13800138000")).to_string_lossy(),
            "导入后应绑定本机路径"
        );
        assert!(!a.block_persist);

        // 导入结果可立即参与派生（设备标识与源机同盐同号 → 相同）
        let b = load(&key_for("13912345678"));
        assert_eq!(b.install_salt, SALT_B);
        let mut expect_src = Credentials {
            phone: "13912345678".to_string(),
            install_salt: SALT_B.to_string(),
            ..Credentials::default()
        };
        expect_src.storage_path = String::new();
        assert_eq!(b.device_id(), expect_src.device_id());
    }

    /// 导入上限：目标已有 2 个，导入含 3 个（其中 1 个是已存在的号）→ 只新增 1 个，超出的跳过。
    #[test]
    fn import_respects_cap_of_three() {
        let src = Isolated::new("cap-src");
        write_cred("13800138000", SALT_A, "at1");
        write_cred("13912345678", SALT_A, "at2");
        write_cred("15212345678", SALT_A, "at3");
        let out = src.dir.join("export.json");
        export_accounts(&out).unwrap();
        let bytes = fs::read(&out).unwrap();
        drop(src);

        let dst = Isolated::new("cap-dst");
        write_cred("13800138000", SALT_A, "local-1"); // 目标已有 2 个
        write_cred("18612345678", SALT_A, "local-2");
        assert_eq!(count(), 2);

        let out2 = dst.dir.join("in.json");
        fs::write(&out2, &bytes).unwrap();
        let summary = import_accounts(&out2).unwrap();
        // 13800138000 已在目标 → 更新（沿用本机盐）；13912345678 新增；15212345678 超上限跳过
        assert_eq!(summary.imported.len(), 2, "同号更新 + 一个新号");
        assert_eq!(count(), 3);
        assert_eq!(summary.skipped.len(), 1);
        assert_eq!(summary.skipped[0].0, "15212345678");
        assert!(summary.skipped[0].1.contains("上限"));
        // 同号更新：本机盐优先（此处两机盐相同，改用另一用例专门验证盐冲突）
        let a = load(&key_for("13800138000"));
        assert_eq!(a.access_token, "at1", "同号导入应更新内容");

        // 再导一次：已在目标的两个号走"更新"（不占名额），15212345678 仍被拒——上限口径稳定
        let summary = import_accounts(&out2).unwrap();
        assert_eq!(summary.imported.len(), 2, "两个同号更新");
        assert_eq!(summary.skipped.len(), 1);
        assert_eq!(summary.skipped[0].0, "15212345678");
        assert_eq!(count(), 3);

        // 腾出一个名额后重导：15212345678 这次能落位
        remove(&key_for("18612345678"));
        assert_eq!(count(), 2);
        let summary = import_accounts(&out2).unwrap();
        assert_eq!(summary.imported.len(), 3);
        assert!(summary.skipped.is_empty());
        assert_eq!(count(), 3);
    }

    /// 导入：盐冲突本机优先（目标已有同号、盐不同 → 保留目标盐）。
    #[test]
    fn import_salt_local_first() {
        let src = Isolated::new("sf-src");
        write_cred("13800138000", SALT_B, "imported-at");
        let out = src.dir.join("export.json");
        export_accounts(&out).unwrap();
        let bytes = fs::read(&out).unwrap();
        drop(src);

        let dst = Isolated::new("sf-dst");
        write_cred("13800138000", SALT_A, "local-at"); // 本机盐 A
        let out2 = dst.dir.join("in.json");
        fs::write(&out2, &bytes).unwrap();
        import_accounts(&out2).unwrap();

        let a = load(&key_for("13800138000"));
        assert_eq!(a.install_salt, SALT_A, "盐冲突须本机优先（设备标识不变）");
        assert_eq!(a.access_token, "imported-at", "内容按导入件更新");
        assert_eq!(
            a.device_id(),
            Credentials {
                phone: "13800138000".to_string(),
                install_salt: SALT_A.to_string(),
                ..Credentials::default()
            }
            .device_id(),
            "设备标识须与本机原值一致"
        );
    }

    /// 导入：非法条目（号码无效/版本不支持/无账号）的报错与跳过。
    #[test]
    fn import_rejects_invalid_inputs() {
        let iso = Isolated::new("bad");

        // 版本不支持
        let p = iso.dir.join("v9.json");
        fs::write(
            &p,
            r#"{"version":9,"exportedAt":1,"accounts":[{"Phone":"13800138000"}]}"#,
        )
        .unwrap();
        assert_eq!(import_accounts(&p), Err(ImportError::UnsupportedVersion(9)));

        // 无账号
        let p = iso.dir.join("empty.json");
        fs::write(&p, r#"{"version":1,"exportedAt":1,"accounts":[]}"#).unwrap();
        assert_eq!(import_accounts(&p), Err(ImportError::NoAccounts));

        // 非 JSON
        let p = iso.dir.join("bad.json");
        fs::write(&p, "not json").unwrap();
        assert!(matches!(import_accounts(&p), Err(ImportError::Read(_))));

        // 号码无效条目被跳过；混合场景下有效条目仍导入
        let p = iso.dir.join("mixed.json");
        fs::write(
            &p,
            r#"{"version":1,"exportedAt":1,"accounts":[
                {"Phone":"1380013","InstallSalt":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
                {"Phone":"13800138000","InstallSalt":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","AccessToken":"ok"}
            ]}"#,
        )
        .unwrap();
        let summary = import_accounts(&p).unwrap();
        assert_eq!(summary.imported, vec!["d4c956e093be53ba"]);
        assert_eq!(summary.skipped.len(), 1);
        assert_eq!(summary.skipped[0].1, "手机号无效");
    }
}
