// Windows 计划任务管理，移植自旧版
// src\GlaccAuto.Core\Scheduling\ScheduledTaskManager.cs。
//
// 旧版核实结论：不是应用内定时循环，而是注册一条每日触发的 Windows 计划任务
// （schtasks，用户级、无需管理员），到点以 `--scheduled` 参数拉起本应用，
// 由 GUI 检测该参数后直接进多账号串行领取并自动退出。
// 注册用 XML 导入而非 /TR（避开路径空格转义，且能显式关掉"仅交流电可启动"）；
// 任务定义关键设置：MultipleInstancesPolicy=IgnoreNew、ExecutionTimeLimit=PT0S（不限时）、
// StartWhenAvailable=false、DisallowStartIfOnBatteries/StopIfGoingOnBatteries=false。
//
// Rust 侧职责：调起封装（std::process::Command）、XML 生成与解析、快照校验、
// 配置解析与保存，以及真正执行注册/注销/查询的入口。
// 注册走 XML 导入（/Create /F /XML），注销走 /Delete /F；两者都只在设置页动作时调用。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde::{Deserialize, Serialize};

use crate::glacc::diag;
use crate::glacc::store;

/// CREATE_NO_WINDOW：schtasks 后台调用不分配控制台窗口（否则开关定时领取会闪黑框）。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 计划任务名（根目录）。
pub const TASK_NAME: &str = "glacc-auto-claim";
/// 到点拉起本应用时传入的参数（GUI 侧据此进入无人值守领取）。
pub const LAUNCH_ARGUMENT: &str = "--scheduled";
/// schtasks 单次调用超时：卡死时终止进程并按失败处理，避免挂住调用方。
pub const SCHTASKS_TIMEOUT: Duration = Duration::from_millis(15_000);

/// 计划任务 XML 命名空间（Task Scheduler 1.2）。
pub const TASK_XML_NS: &str = "http://schemas.microsoft.com/windows/2004/02/mit/task";

/// 注册/注销失败：消息可直接展示给用户。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledTaskError(pub String);

impl std::fmt::Display for ScheduledTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ScheduledTaskError {}

/// 一天内的触发时刻（时、分）。用自建类型而非 chrono：本项目零新增依赖。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeOfDay {
    pub hour: u8,
    pub minute: u8,
}

impl TimeOfDay {
    pub fn new(hour: u32, minute: u32) -> Option<Self> {
        if hour > 23 || minute > 59 {
            return None;
        }
        Some(Self {
            hour: hour as u8,
            minute: minute as u8,
        })
    }

    /// 解析 "HH:MM" / "H:MM"（旧版 AppSettings.ScheduledTime 的格式）。
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let (h, m) = s.split_once(':')?;
        let hour: u32 = h.trim().parse().ok()?;
        let minute: u32 = m.trim().parse().ok()?;
        Self::new(hour, minute)
    }

    pub fn as_hhmm(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }
}

impl Default for TimeOfDay {
    fn default() -> Self {
        // 旧版 AppSettings.ScheduledTime 默认值
        Self {
            hour: 8,
            minute: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    Register,
    Unregister,
}

/// 一次 schtasks 调用的描述（本步只构造、不执行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchtasksCall {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl SchtasksCall {
    /// 便于日志/测试断言的命令行形态（不做 Windows 引号转义，仅用于展示）。
    pub fn display(&self) -> String {
        format!("{} {}", self.program.display(), self.args.join(" "))
    }
}

/// 执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchtasksOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// schtasks 调用抽象：生产实现走 std::process::Command，测试注入 Mock（绝不调真实计划任务）。
pub trait SchtasksRunner {
    fn run(&self, call: &SchtasksCall) -> Result<SchtasksOutput, ScheduledTaskError>;
}

/// 生产实现：定位 %SystemRoot%\System32\schtasks.exe，重定向输出，超时强杀。
pub struct SystemSchtasks;

impl SchtasksRunner for SystemSchtasks {
    fn run(&self, call: &SchtasksCall) -> Result<SchtasksOutput, ScheduledTaskError> {
        let mut child = Command::new(&call.program)
            .args(&call.args)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ScheduledTaskError(format!("启动 schtasks 失败：{e}")))?;

        // 两个管道必须并发排空：Windows 管道缓冲区约 64KB，查询全量任务列表
        // （/Query /FO CSV /NH）很容易超限；先等进程退出再读会互相卡死到超时。
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let out_reader = stdout_pipe.map(|mut p| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = p.read_to_end(&mut buf);
                buf
            })
        });
        let err_reader = stderr_pipe.map(|mut p| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = p.read_to_end(&mut buf);
                buf
            })
        });

        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if start.elapsed() > SCHTASKS_TIMEOUT {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(ScheduledTaskError("schtasks 调用超时".to_string()));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    return Err(ScheduledTaskError(format!("等待 schtasks 失败：{e}")))
                }
            }
        };

        let stdout = out_reader
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let stderr = err_reader
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        Ok(SchtasksOutput {
            exit_code: status.code().unwrap_or(-1),
            stdout: decode_console(&stdout),
            stderr: decode_console(&stderr),
        })
    }
}

/// 控制台输出解码。schtasks 的输出编码不固定，实测三种都会出现：
/// - `/Query /XML` 走 UTF-8（尽管 XML 声明写着 UTF-16）；
/// - 报错文案随当前代码页（中文 Windows 为 GBK/OEM 936）；
/// - 个别宿主下（重定向到文件等）可能给出带 BOM 的 UTF-16。
/// 判定顺序：BOM → 无 BOM 的 UTF-16 启发式 → 严格 UTF-8 → 系统 ANSI 代码页。
fn decode_console(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16_lossy(&bytes[2..], true);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16_lossy(&bytes[2..], false);
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&bytes[3..]).to_string();
    }
    if looks_like_utf16le(bytes) {
        let decoded = decode_utf16_lossy(bytes, true);
        if !decoded.contains('\u{FFFD}') && !decoded.contains('\0') {
            return decoded;
        }
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => decode_ansi(bytes),
    }
}

/// 无 BOM 的 UTF-16LE 启发式：ASCII 字符占多数时高位字节为 0，
/// 于是"偶数长度 + 奇数位出现 NUL"是很强的信号（GBK/UTF-8 正文里不会出现 NUL）。
fn looks_like_utf16le(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && bytes.len() % 2 == 0
        && bytes
            .iter()
            .enumerate()
            .any(|(i, b)| i % 2 == 1 && *b == 0)
}

/// UTF-16 解码，非法码元替换为 U+FFFD。
fn decode_utf16_lossy(bytes: &[u8], little_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

#[allow(non_snake_case)]
#[link(name = "kernel32")]
extern "system" {
    fn MultiByteToWideChar(
        code_page: u32,
        dw_flags: u32,
        lp_multi_byte_str: *const u8,
        cb_multi_byte: i32,
        lp_wide_char_str: *mut u16,
        cch_wide_char: i32,
    ) -> i32;
}

/// 按系统 ANSI 代码页解码（CP_ACP=0；中文 Windows 即 GBK/936）。
/// 旧版用 CultureInfo.CurrentCulture.TextInfo.OEMCodePage，语义等价；
/// 只在日志与错误文案上使用，解码失败回退替换字符，绝不影响业务流程。
fn decode_ansi(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let len = bytes.len().min(i32::MAX as usize) as i32;
    // SAFETY: 只读入参缓冲，指针在调用期间有效；先探长度再取内容。
    let needed = unsafe {
        MultiByteToWideChar(0, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0)
    };
    if needed <= 0 {
        return String::from_utf8_lossy(bytes).to_string();
    }
    let mut buf = vec![0u16; needed as usize];
    let written = unsafe {
        MultiByteToWideChar(0, 0, bytes.as_ptr(), len, buf.as_mut_ptr(), needed)
    };
    if written <= 0 {
        return String::from_utf8_lossy(bytes).to_string();
    }
    buf.truncate(written as usize);
    String::from_utf16_lossy(&buf)
}

/// 任务定义快照，取自 schtasks /Query /XML。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub enabled: bool,
    pub trigger_enabled: bool,
    pub daily: bool,
    pub start_time: TimeOfDay,
    pub command: String,
    pub arguments: String,
    pub start_when_available: bool,
    pub disallow_start_if_on_batteries: bool,
    pub stop_if_going_on_batteries: bool,
    pub no_execution_time_limit: bool,
}

/// 快照是否与预期一致：每日 time 时刻、指定 exe + --scheduled、任务与触发器均启用，
/// 且四项设置符合预期（与旧版 MatchesExpectation 逐条对应）。
pub fn matches_expectation(snapshot: &TaskSnapshot, time: TimeOfDay, exe: &Path) -> bool {
    let expected_exe = exe.to_string_lossy();
    snapshot.enabled
        && snapshot.trigger_enabled
        && snapshot.daily
        && snapshot.start_time.hour == time.hour
        && snapshot.start_time.minute == time.minute
        && snapshot.command.trim_matches('"').eq_ignore_ascii_case(&expected_exe)
        && snapshot.arguments.trim() == LAUNCH_ARGUMENT
        && !snapshot.start_when_available
        && !snapshot.disallow_start_if_on_batteries
        && !snapshot.stop_if_going_on_batteries
        && snapshot.no_execution_time_limit
}

/// 注册用 XML（与旧版 BuildTaskXml 逐项一致；日期部分由调用方给定，便于测试固定）。
pub fn build_task_xml(exe: &Path, time: TimeOfDay, date: &str) -> String {
    let start = format!("{date}T{:02}:{:02}:00", time.hour, time.minute);
    let escaped = escape_xml(&exe.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="{TASK_XML_NS}">
  <RegistrationInfo>
    <Description>glacc-auto 每日定时领取</Description>
  </RegistrationInfo>
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>{start}</StartBoundary>
      <Enabled>true</Enabled>
      <ScheduleByDay>
        <DaysInterval>1</DaysInterval>
      </ScheduleByDay>
    </CalendarTrigger>
  </Triggers>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>false</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{escaped}</Command>
      <Arguments>{LAUNCH_ARGUMENT}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// XML 转义（旧版用 SecurityElement.Escape，替换集合一致：& < > " '）。
fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// 只读极简 XML 取值：按标签名取首个文本内容（去空白）。
/// 任务 XML 结构固定且由 Windows 生成，无需完整 XML 解析器；
/// 无法解析（结构不符）时返回 None，由调用方按"状态无法判定"处理。
fn element_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = xml.find(&open)?;
    let after_open = xml[start..].find('>')? + start + 1;
    let end = xml[after_open..].find(&close)? + after_open;
    Some(xml[after_open..end].trim().to_string())
}

/// 取某个父元素内的子元素文本（避免同名标签跨段误匹配）。
fn child_text(xml: &str, parent: &str, child: &str) -> Option<String> {
    let block = block_of(xml, parent)?;
    element_text(&block, child)
}

/// 取首个 `<parent ...>...</parent>` 片段（含标签）。
fn block_of(xml: &str, parent: &str) -> Option<String> {
    let open = format!("<{parent}");
    let close = format!("</{parent}>");
    let start = xml.find(&open)?;
    let end = xml[start..].find(&close)? + start + close.len();
    Some(xml[start..end].to_string())
}

/// ExecutionTimeLimit 是否等价于"不限时"：缺省（Windows 默认即不限时）或 PT0S。
/// 老版本任务写死 PT2H，多账号串行跑很容易超时被杀，需判为不符并重注册纠正。
fn is_unlimited(limit: &str) -> bool {
    limit.is_empty() || limit.eq_ignore_ascii_case("PT0S")
}

/// 解析 StartBoundary（如 "2026-10-17T08:00:00"）中的时刻。
fn parse_start_time(s: &str) -> Option<TimeOfDay> {
    let t = s.split_once('T')?.1;
    let mut parts = t.split(':');
    let hour: u32 = parts.next()?.parse().ok()?;
    let minute: u32 = parts.next()?.parse().ok()?;
    TimeOfDay::new(hour, minute)
}

/// 解析 schtasks /Query /XML 的导出内容。
/// 返回 Err = 结构不符（无法判定状态）；与旧版 TryQuery 的 false 分支对应。
pub fn parse_task_xml(xml: &str) -> Result<TaskSnapshot, ScheduledTaskError> {
    let trigger = block_of(xml, "CalendarTrigger")
        .ok_or_else(|| ScheduledTaskError("任务 XML 缺少 CalendarTrigger".to_string()))?;
    let exec = block_of(xml, "Exec")
        .ok_or_else(|| ScheduledTaskError("任务 XML 缺少 Exec".to_string()))?;
    let settings = block_of(xml, "Settings")
        .ok_or_else(|| ScheduledTaskError("任务 XML 缺少 Settings".to_string()))?;

    let start_raw = element_text(&trigger, "StartBoundary")
        .ok_or_else(|| ScheduledTaskError("任务 XML 缺少 StartBoundary".to_string()))?;
    let start_time = parse_start_time(&start_raw)
        .ok_or_else(|| ScheduledTaskError(format!("StartBoundary 无法解析：{start_raw}")))?;

    let daily = child_text(&trigger, "ScheduleByDay", "DaysInterval")
        .and_then(|v| v.parse::<i32>().ok())
        .is_some_and(|n| n == 1);

    // 与旧版口径一致：导出 XML 中 Enabled 启用时缺省、禁用时才写 false
    let enabled = element_text(&settings, "Enabled").as_deref() != Some("false");
    let trigger_enabled = element_text(&trigger, "Enabled").as_deref() != Some("false");

    Ok(TaskSnapshot {
        enabled,
        trigger_enabled,
        daily,
        start_time,
        command: element_text(&exec, "Command").unwrap_or_default(),
        arguments: element_text(&exec, "Arguments").unwrap_or_default(),
        start_when_available: element_text(&settings, "StartWhenAvailable").as_deref()
            == Some("true"),
        disallow_start_if_on_batteries: element_text(&settings, "DisallowStartIfOnBatteries")
            .as_deref()
            != Some("false"),
        stop_if_going_on_batteries: element_text(&settings, "StopIfGoingOnBatteries").as_deref()
            != Some("false"),
        no_execution_time_limit: is_unlimited(
            &element_text(&settings, "ExecutionTimeLimit").unwrap_or_default(),
        ),
    })
}

// ── 计划任务配置（启用与否 + 时间点），落盘到数据根目录 ──

/// 计划任务配置：复刻旧版 AppSettings 中参与定时的两个字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleConfig {
    /// 是否注册计划任务
    #[serde(rename = "ScheduledEnabled")]
    pub enabled: bool,
    /// 触发时刻 "HH:MM"
    #[serde(rename = "ScheduledTime")]
    pub time: String,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            time: TimeOfDay::default().as_hhmm(),
        }
    }
}

impl ScheduleConfig {
    /// 读取时的可信化（旧版 Normalize 的对应部分）：
    /// 时间非法回退默认 08:00，绝不把坏值带进 schtasks。
    pub fn normalize(&mut self) {
        let parsed = TimeOfDay::parse(&self.time);
        self.time = parsed.unwrap_or_default().as_hhmm();
    }

    pub fn start_time(&self) -> TimeOfDay {
        TimeOfDay::parse(&self.time).unwrap_or_default()
    }

    pub fn from_json(json: &str) -> Result<Self, ScheduledTaskError> {
        let mut cfg: ScheduleConfig = serde_json::from_str(json)
            .map_err(|e| ScheduledTaskError(format!("计划任务配置无法解析：{e}")))?;
        cfg.normalize();
        Ok(cfg)
    }

    pub fn to_json(&self) -> Result<String, ScheduledTaskError> {
        let mut cfg = self.clone();
        cfg.normalize();
        serde_json::to_string_pretty(&cfg)
            .map_err(|e| ScheduledTaskError(format!("计划任务配置无法序列化：{e}")))
    }

    /// 配置文件路径：与凭据同一数据根目录。
    pub fn path() -> PathBuf {
        store::data_root().join("schedule.json")
    }

    /// 读取；文件缺失或内容损坏 → 默认值（与旧版 AppSettings.Load 的容错一致）。
    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match Self::from_json(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    store::log(&format!("{e}，本次使用默认值：{}", path.display()));
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                store::log(&format!("读取计划任务配置失败（{e}），本次使用默认值"));
                Self::default()
            }
        }
    }

    /// 原子落盘（复用 store 的临时文件 + 改名；失败只记日志，不阻断业务）。
    pub fn save(&self) {
        let path = Self::path();
        match self.to_json() {
            Ok(json) => {
                if let Err(e) = store::write_atomic(&path, &json) {
                    store::log(&format!("保存计划任务配置失败：{e}"));
                }
            }
            Err(e) => store::log(&e.0),
        }
    }
}

// ── 调起与真实执行 ──

/// schtasks.exe 绝对路径（旧版 Path.Combine(Environment.SystemDirectory, "schtasks.exe")）。
pub fn schtasks_path() -> PathBuf {
    let root = std::env::var("SystemRoot")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "C:\\Windows".to_string());
    Path::new(&root).join("System32").join("schtasks.exe")
}

/// `/Query /TN "<name>" /XML`
pub fn query_call() -> SchtasksCall {
    SchtasksCall {
        program: schtasks_path(),
        args: vec![
            "/Query".to_string(),
            "/TN".to_string(),
            TASK_NAME.to_string(),
            "/XML".to_string(),
        ],
    }
}

/// 不带 /TN 的全量列表查询（CSV）：只用来判定任务是否存在。
pub fn list_call() -> SchtasksCall {
    SchtasksCall {
        program: schtasks_path(),
        args: vec![
            "/Query".to_string(),
            "/FO".to_string(),
            "CSV".to_string(),
            "/NH".to_string(),
        ],
    }
}

/// `/Create /F /TN "<name>" /XML "<file>"`
pub fn create_call(xml_path: &Path) -> SchtasksCall {
    SchtasksCall {
        program: schtasks_path(),
        args: vec![
            "/Create".to_string(),
            "/F".to_string(),
            "/TN".to_string(),
            TASK_NAME.to_string(),
            "/XML".to_string(),
            xml_path.to_string_lossy().to_string(),
        ],
    }
}

/// `/Delete /F /TN "<name>"`
pub fn delete_call() -> SchtasksCall {
    SchtasksCall {
        program: schtasks_path(),
        args: vec![
            "/Delete".to_string(),
            "/F".to_string(),
            "/TN".to_string(),
            TASK_NAME.to_string(),
        ],
    }
}

/// 按配置注册或注销（只表达意图，实际执行走 register / unregister）。
pub fn plan_launch(cfg: &ScheduleConfig, exe: &Path) -> Launch {
    let _ = (cfg, exe);
    if cfg.enabled {
        Launch::Register
    } else {
        Launch::Unregister
    }
}

/// 命令行里是否带 `--scheduled`（大小写不敏感，与旧版一致）。
pub fn is_scheduled_launch<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .any(|a| a.as_ref().eq_ignore_ascii_case(LAUNCH_ARGUMENT))
}

/// 注册/注销/查询串行门：schtasks 是同步外部调用（最长 15 秒），
/// 并发的注册与注销会互相覆盖，统一串行化。
static SCHEDULE_GATE: Mutex<()> = Mutex::new(());

fn gate() -> std::sync::MutexGuard<'static, ()> {
    SCHEDULE_GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 失败文案：优先 stderr，空则用 stdout，再空则给退出码（旧版 Describe）。
fn describe(out: &SchtasksOutput) -> String {
    let text = out.stderr.trim();
    if !text.is_empty() {
        return text.to_string();
    }
    let text = out.stdout.trim();
    if !text.is_empty() {
        return text.to_string();
    }
    format!("schtasks 退出码 {}", out.exit_code)
}

/// 今天日期（yyyy-MM-dd），StartBoundary 的日期部分。
fn today_stamp() -> String {
    diag::local_timestamp().chars().take(10).collect()
}

/// 按 UTF-16LE + BOM 写任务定义：XML 声明是 encoding="UTF-16"，
/// schtasks 会按声明解码；UTF-8 文件会被它按 UTF-16 读成乱码。
fn write_utf16(path: &Path, text: &str) -> Result<(), ScheduledTaskError> {
    let mut bytes = Vec::with_capacity(text.len() * 2 + 2);
    bytes.extend_from_slice(&[0xFF, 0xFE]);
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(path, bytes).map_err(|e| ScheduledTaskError(format!("写入任务定义失败：{e}")))
}

/// 列表查询判定任务是否存在。None = 列表查询本身失败，状态无法判定。
fn task_exists_with(runner: &dyn SchtasksRunner) -> Option<bool> {
    let out = runner.run(&list_call()).ok()?;
    if out.exit_code != 0 {
        return None;
    }
    Some(csv_names_task(&out.stdout))
}

/// CSV 首列为任务路径名，根目录任务形如 "\glacc-auto-claim"。
fn csv_names_task(csv: &str) -> bool {
    for line in csv.lines() {
        let Some(start) = line.find('"') else {
            continue;
        };
        let after = &line[start + 1..];
        let Some(end) = after.find('"') else {
            continue;
        };
        let name = after[..end].trim_start_matches('\\');
        if name.eq_ignore_ascii_case(TASK_NAME) {
            return true;
        }
    }
    false
}

/// 查询任务定义（调用方须已持门）。
/// Ok(None) = 确认任务不存在；Err = 查询本身失败或定义取不到，状态无法判定。
fn query_locked(runner: &dyn SchtasksRunner) -> Result<Option<TaskSnapshot>, ScheduledTaskError> {
    let out = runner.run(&query_call())?;
    if out.exit_code == 0 {
        return parse_task_xml(&out.stdout).map(Some);
    }
    // 非零退出码既可能是"任务不存在"，也可能是"访问被拒/服务异常"，
    // 且报错文案随宿主代码页变化不可靠：改用列表查询查证存在性。
    match task_exists_with(runner) {
        Some(false) => Ok(None),
        Some(true) => Err(ScheduledTaskError(format!(
            "计划任务存在但定义取不到：{}",
            describe(&out)
        ))),
        None => Err(ScheduledTaskError(format!(
            "计划任务状态无法判定：{}",
            describe(&out)
        ))),
    }
}

/// 查询任务定义。Ok(None) = 任务不存在。
pub fn query_snapshot() -> Result<Option<TaskSnapshot>, ScheduledTaskError> {
    let _guard = gate();
    query_locked(&SystemSchtasks)
}

/// 注册动作本体（调用方须已持门）：写临时 XML → /Create /F → 删临时文件。
fn create_locked(
    runner: &dyn SchtasksRunner,
    exe: &Path,
    time: TimeOfDay,
) -> Result<(), ScheduledTaskError> {
    let xml = build_task_xml(exe, time, &today_stamp());
    // 文件名带进程号：前台实例与计划任务拉起的实例可能同时注册，固定名会互相删对方正在用的文件。
    let tmp = std::env::temp_dir().join(format!(
        "glacc-auto-task-{}-{}.xml",
        std::process::id(),
        tmp_salt()
    ));
    write_utf16(&tmp, &xml)?;
    let result = runner.run(&create_call(&tmp));
    let _ = std::fs::remove_file(&tmp);
    let out = result?;
    if out.exit_code != 0 {
        diag::warn(&format!("注册计划任务失败：{}", describe(&out)));
        return Err(ScheduledTaskError(describe(&out)));
    }
    diag::info(&format!(
        "已注册计划任务 {TASK_NAME}：每天 {} 拉起 {}",
        time.as_hhmm(),
        exe.display()
    ));
    Ok(())
}

/// 临时文件名去重用的纳秒尾数：同一进程内连续两次注册（改时间连点）也不会撞名。
fn tmp_salt() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u128 + d.as_secs() as u128 * 1_000_000_000)
        .unwrap_or(0)
}

/// 按当前 exe 与指定时刻注册任务（/F 幂等覆盖）。
pub fn register(exe: &Path, time: TimeOfDay) -> Result<(), ScheduledTaskError> {
    register_with(&SystemSchtasks, exe, time)
}

/// 注册（可注入调用器，测试用）。
pub fn register_with(
    runner: &dyn SchtasksRunner,
    exe: &Path,
    time: TimeOfDay,
) -> Result<(), ScheduledTaskError> {
    let _guard = gate();
    create_locked(runner, exe, time)
}

/// 注销任务；任务确认不存在视为已注销（幂等）。
pub fn unregister() -> Result<(), ScheduledTaskError> {
    unregister_with(&SystemSchtasks)
}

/// 注销（可注入调用器，测试用）。
pub fn unregister_with(runner: &dyn SchtasksRunner) -> Result<(), ScheduledTaskError> {
    let _guard = gate();
    // 无条件先删：查询失败（瞬时故障）不应把仍在的任务误判为已注销而跳过删除
    let out = runner.run(&delete_call())?;
    if out.exit_code == 0 {
        diag::info(&format!("已删除计划任务 {TASK_NAME}"));
        return Ok(());
    }
    match task_exists_with(runner) {
        Some(false) => {
            diag::info(&format!("计划任务 {TASK_NAME} 不存在，无需删除"));
            Ok(())
        }
        _ => {
            diag::warn(&format!("删除计划任务失败：{}", describe(&out)));
            Err(ScheduledTaskError(describe(&out)))
        }
    }
}

/// 启动时对齐：任务缺失或与当前 exe/时刻不符则重注册；开关关闭则什么都不做
/// （不擅自删除用户手工建的同名任务）。
pub fn reconcile_at_startup(exe: &Path) -> Result<(), ScheduledTaskError> {
    reconcile_with(&SystemSchtasks, exe)
}

/// 启动对齐（可注入调用器，测试用）。
pub fn reconcile_with(
    runner: &dyn SchtasksRunner,
    exe: &Path,
) -> Result<(), ScheduledTaskError> {
    let cfg = ScheduleConfig::load();
    if !cfg.enabled {
        return Ok(());
    }
    let time = cfg.start_time();
    let _guard = gate();
    match query_locked(runner)? {
        Some(snapshot) if matches_expectation(&snapshot, time, exe) => Ok(()),
        Some(_) => {
            // 开发构建不覆盖已存在的任务：避免 debug 产物抢走正式安装版的任务指向
            if cfg!(debug_assertions) {
                diag::warn("计划任务与当前程序不一致；开发构建不自动重注册");
                return Ok(());
            }
            diag::info("计划任务与当前设置不一致，重新注册");
            create_locked(runner, exe, time)
        }
        None => {
            if cfg!(debug_assertions) {
                diag::warn("定时领取已开启但计划任务不存在；开发构建不自动注册");
                return Ok(());
            }
            diag::info("定时领取已开启但计划任务不存在，补注册");
            create_locked(runner, exe, time)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 配置解析：固定 JSON → 结构正确；坏时间回退 08:00；缺字段用默认值。
    #[test]
    fn schedule_config_json_parsing() {
        let cfg = ScheduleConfig::from_json(
            r#"{"ScheduledEnabled": true, "ScheduledTime": "21:35"}"#,
        )
        .unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.time, "21:35");
        assert_eq!(
            cfg.start_time(),
            TimeOfDay {
                hour: 21,
                minute: 35
            }
        );

        // 时间非法 → 回退默认，不把坏值带进 schtasks
        let bad =
            ScheduleConfig::from_json(r#"{"ScheduledEnabled": true, "ScheduledTime": "99:99"}"#)
                .unwrap();
        assert_eq!(bad.time, "08:00");

        let garbage =
            ScheduleConfig::from_json(r#"{"ScheduledTime": "not-a-time"}"#).unwrap();
        assert_eq!(garbage.time, "08:00");
        assert!(!garbage.enabled);

        // 缺字段：serde(default) → 默认值
        let empty = ScheduleConfig::from_json("{}").unwrap();
        assert_eq!(empty, ScheduleConfig::default());

        // 非法 JSON：报错
        assert!(ScheduleConfig::from_json("{oops").is_err());

        // 往返
        let round = ScheduleConfig::from_json(&cfg.to_json().unwrap()).unwrap();
        assert_eq!(round, cfg);
    }

    /// TimeOfDay 边界。
    #[test]
    fn time_of_day_bounds() {
        assert_eq!(TimeOfDay::parse("08:00"), TimeOfDay::new(8, 0));
        assert_eq!(TimeOfDay::parse("8:5"), TimeOfDay::new(8, 5));
        assert_eq!(TimeOfDay::parse("23:59"), TimeOfDay::new(23, 59));
        assert_eq!(TimeOfDay::parse("24:00"), None);
        assert_eq!(TimeOfDay::parse("12:60"), None);
        assert_eq!(TimeOfDay::parse(""), None);
        assert_eq!(TimeOfDay::parse("1200"), None);
        assert_eq!(TimeOfDay::default().as_hhmm(), "08:00");
    }

    /// 注册 XML 与旧版逐项一致（含转义、固定设置、--scheduled）。
    #[test]
    fn task_xml_matches_legacy() {
        let xml = build_task_xml(
            Path::new(r"C:\Program Files\glacc & co\glacc-auto.exe"),
            TimeOfDay::new(8, 30).unwrap(),
            "2026-10-17",
        );
        assert!(xml.contains(r#"xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task""#));
        assert!(xml.contains("<StartBoundary>2026-10-17T08:30:00</StartBoundary>"));
        assert!(xml.contains("<DaysInterval>1</DaysInterval>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<StartWhenAvailable>false</StartWhenAvailable>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains("--scheduled"));
        assert!(
            xml.contains(r"C:\Program Files\glacc &amp; co\glacc-auto.exe"),
            "exe 路径必须 XML 转义"
        );
        assert!(!xml.contains("& co"), "未转义的 & 不应出现");
    }

    /// 查询 XML 解析 + MatchesExpectation：正常任务判定一致；
    /// ExecutionTimeLimit 缺省或 PT0S 视为不限时，PT2H 判为不符。
    #[test]
    fn parse_query_xml_and_expectation() {
        let exe = Path::new(r"C:\Apps\glacc-auto.exe");
        let xml = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers>
    <CalendarTrigger>
      <StartBoundary>2026-10-17T08:00:00</StartBoundary>
      <ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>
    </CalendarTrigger>
  </Triggers>
  <Settings>
    <StartWhenAvailable>false</StartWhenAvailable>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>C:\Apps\glacc-auto.exe</Command>
      <Arguments>--scheduled</Arguments>
    </Exec>
  </Actions>
</Task>"#;
        let snap = parse_task_xml(xml).unwrap();
        assert!(snap.enabled);
        assert!(snap.trigger_enabled);
        assert!(snap.daily);
        assert_eq!(
            snap.start_time,
            TimeOfDay {
                hour: 8,
                minute: 0
            }
        );
        assert_eq!(snap.command, r"C:\Apps\glacc-auto.exe");
        assert_eq!(snap.arguments, "--scheduled");
        assert!(snap.no_execution_time_limit);
        assert!(matches_expectation(&snap, TimeOfDay::default(), exe));

        // 时刻不符
        assert!(!matches_expectation(
            &snap,
            TimeOfDay::new(9, 0).unwrap(),
            exe
        ));
        // exe 不同
        assert!(!matches_expectation(
            &snap,
            TimeOfDay::default(),
            Path::new(r"C:\Other\glacc-auto.exe")
        ));

        // 老任务写死 PT2H → 判为不符（需重注册纠正）
        let legacy_limit = xml.replace("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>", "<ExecutionTimeLimit>PT2H</ExecutionTimeLimit>");
        let snap2 = parse_task_xml(&legacy_limit).unwrap();
        assert!(!snap2.no_execution_time_limit);
        assert!(!matches_expectation(&snap2, TimeOfDay::default(), exe));

        // ExecutionTimeLimit 缺省 = 不限时（Windows 默认）
        let no_limit = xml.replace(
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "",
        );
        assert!(parse_task_xml(&no_limit).unwrap().no_execution_time_limit);

        // 禁用状态：Enabled=false 判为不符
        let disabled = xml.replace("<Enabled>true</Enabled>", "<Enabled>false</Enabled>");
        let snap3 = parse_task_xml(&disabled).unwrap();
        assert!(!snap3.enabled, "Enabled=false 应判为未启用");
        assert!(!matches_expectation(&snap3, TimeOfDay::default(), exe));

        // 结构缺失 → 报错（对应旧版"状态无法判定"）
        assert!(parse_task_xml("<Task></Task>").is_err());
    }

    /// 命令构造：参数与旧版逐项一致，且都是绝对路径调用（不依赖 PATH）。
    #[test]
    fn schtasks_calls_match_legacy() {
        let q = query_call();
        assert_eq!(
            q.args,
            vec!["/Query", "/TN", TASK_NAME, "/XML"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert!(q.program.ends_with("System32\\schtasks.exe")
            || q.program.ends_with("System32/schtasks.exe"));

        let c = create_call(Path::new(r"C:\Temp\task.xml"));
        assert_eq!(c.args[0], "/Create");
        assert_eq!(c.args[1], "/F");
        assert_eq!(c.args[2], "/TN");
        assert_eq!(c.args[3], TASK_NAME);
        assert_eq!(c.args[4], "/XML");
        assert_eq!(c.args[5], r"C:\Temp\task.xml");

        let d = delete_call();
        assert_eq!(d.args, vec!["/Delete", "/F", "/TN", TASK_NAME]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>());

        // plan_launch 只按开关决定动作
        let mut cfg = ScheduleConfig::default();
        assert_eq!(plan_launch(&cfg, Path::new("x.exe")), Launch::Unregister);
        cfg.enabled = true;
        assert_eq!(plan_launch(&cfg, Path::new("x.exe")), Launch::Register);
    }

    /// 命令行识别 --scheduled（大小写不敏感，与旧版一致）。
    #[test]
    fn scheduled_launch_argument_is_recognized() {
        assert!(is_scheduled_launch(["--scheduled"]));
        assert!(is_scheduled_launch(["--SCHEDULED"]));
        assert!(is_scheduled_launch(["--foo", "--Scheduled", "x"]));
        assert!(!is_scheduled_launch(["--schedule"]));
        assert!(!is_scheduled_launch(Vec::<String>::new()));
    }

    /// 任务定义必须是 UTF-16LE + BOM：XML 声明为 UTF-16，写错编码 schtasks 读成乱码。
    #[test]
    fn task_xml_file_is_utf16_with_bom() {
        let dir = std::env::temp_dir().join(format!("glacc-sched-utf16-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("task.xml");
        let text = "<Task>\u{4e2d}\u{6587}</Task>";
        write_utf16(&path, text).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xFE], "必须带 UTF-16LE BOM");
        assert_eq!(bytes.len(), 2 + text.encode_utf16().count() * 2);
        assert_eq!(decode_utf16_lossy(&bytes[2..], true), text, "往返一致");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// schtasks /FO CSV /NH 的存在性解析：根目录任务名带反斜杠前缀，大小写不敏感。
    #[test]
    fn csv_list_detects_task_name() {
        let csv = "\"\\Microsoft\\Windows\\Foo\",\"N/A\",\"Ready\"\n\"\\glacc-auto-claim\",\"N/A\",\"Ready\"\n";
        assert!(csv_names_task(csv));
        assert!(csv_names_task("\"\\GLACC-AUTO-CLAIM\",\"N/A\",\"Ready\""));
        assert!(!csv_names_task("\"\\Microsoft\\Windows\\Foo\",\"N/A\",\"Ready\""));
        assert!(!csv_names_task(""));
    }

    /// 控制台解码：UTF-16（带/不带 BOM）、UTF-8、ANSI 三种输入都能还原。
    #[test]
    fn console_output_decoding_handles_utf16_and_utf8() {
        let text = "任务 已创建";
        // UTF-16LE + BOM
        let mut le = vec![0xFF, 0xFE];
        for unit in text.encode_utf16() {
            le.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_console(&le), text);
        // 无 BOM 的 UTF-16LE（ASCII 高字节全 0 时按启发式识别）
        let mut bare = Vec::new();
        for unit in text.encode_utf16() {
            bare.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_console(&bare), text);
        // 纯 UTF-8（探查确认 /Query /XML 的输出就是这一路，尽管声明写着 UTF-16）
        assert_eq!(decode_console("abc 错误".as_bytes()), "abc 错误");
        assert_eq!(decode_console(b"<?xml version=\"1.0\"?>"), "<?xml version=\"1.0\"?>");
        // ANSI/GBK（中文 Windows 上的 stderr 报错）不得出现替换字符
        let gbk_err = [0xB4, 0xED, 0xCE, 0xF3]; // "错误"
        let decoded = decode_console(&gbk_err);
        assert!(!decoded.is_empty() && !decoded.contains('\u{FFFD}'), "decoded={decoded}");
        assert_eq!(decode_console(b""), "");
    }

    /// 脚本化调用器：记录每次命令行，并可按顺序返回预设输出（绝不调真实 schtasks）。
    struct MockRunner {
        calls: Mutex<Vec<SchtasksCall>>,
        script: Mutex<std::collections::VecDeque<SchtasksOutput>>,
        /// 捕获 /Create 时临时 XML 的内容（用 UTF-16 解码），供断言注册内容
        seen_xml: Mutex<Option<String>>,
    }

    impl MockRunner {
        fn new(script: Vec<SchtasksOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                script: Mutex::new(script.into()),
                seen_xml: Mutex::new(None),
            }
        }

        /// 固定返回同一条输出（调用次数不限）。
        fn always(out: SchtasksOutput) -> Self {
            Self::new(vec![out.clone()])
        }

        fn calls(&self) -> Vec<SchtasksCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl SchtasksRunner for MockRunner {
        fn run(&self, call: &SchtasksCall) -> Result<SchtasksOutput, ScheduledTaskError> {
            self.calls.lock().unwrap().push(call.clone());
            if let Some(arg) = call.args.last().filter(|a| a.ends_with(".xml")) {
                let bytes = std::fs::read(arg).unwrap_or_default();
                *self.seen_xml.lock().unwrap() = Some(decode_utf16_lossy(&bytes[2..], true));
            }
            let mut script = self.script.lock().unwrap();
            if script.len() > 1 {
                Ok(script.pop_front().unwrap())
            } else {
                Ok(script.front().cloned().unwrap_or(SchtasksOutput {
                    exit_code: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                }))
            }
        }
    }

    fn ok_output(stdout: &str) -> SchtasksOutput {
        SchtasksOutput {
            exit_code: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    fn err_output(stderr: &str) -> SchtasksOutput {
        SchtasksOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    /// 注册：命令行是 /Create /F /TN <name> /XML <tmp>，临时 XML 已删、内容含 exe 与时刻。
    #[test]
    fn register_creates_task_and_cleans_temp_file() {
        let runner = MockRunner::always(ok_output("成功"));
        let exe = Path::new(r"C:\Apps\glacc-auto.exe");
        register_with(&runner, exe, TimeOfDay::new(21, 35).unwrap()).unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "只应发出一次 /Create");
        let args = &calls[0].args;
        assert_eq!(args[0], "/Create");
        assert_eq!(args[1], "/F");
        assert_eq!(args[3], TASK_NAME);
        let tmp = &args[5];
        assert!(tmp.ends_with(".xml"), "tmp={tmp}");
        assert!(!Path::new(tmp).exists(), "临时 XML 必须删除");

        let xml = runner.seen_xml.lock().unwrap().clone().unwrap();
        assert!(xml.contains(r"C:\Apps\glacc-auto.exe"));
        assert!(xml.contains("T21:35:00"));
        assert!(xml.contains("--scheduled"));
    }

    /// 注册失败：返回错误文案（来自 stderr），且临时文件仍被清理。
    #[test]
    fn register_failure_surfaces_stderr() {
        let runner = MockRunner::always(err_output("访问被拒绝"));
        let err = register_with(
            &runner,
            Path::new(r"C:\Apps\glacc-auto.exe"),
            TimeOfDay::default(),
        )
        .unwrap_err();
        assert!(err.0.contains("访问被拒绝"), "err={}", err.0);
    }

    /// 注销：删除成功即结束；删除失败但任务确实不存在同样视为成功（幂等）。
    #[test]
    fn unregister_is_idempotent() {
        // 1) 删除成功
        let runner = MockRunner::always(ok_output("成功"));
        unregister_with(&runner).unwrap();
        assert_eq!(runner.calls().len(), 1);

        // 2) 删除失败 + 列表确认不存在 → 幂等成功
        let runner = MockRunner::new(vec![
            err_output("找不到指定的文件"),
            ok_output("\"\\Microsoft\\Windows\\Foo\",\"N/A\"\n"),
        ]);
        unregister_with(&runner).unwrap();
        assert_eq!(runner.calls().len(), 2, "应删一次再查一次列表");
        assert_eq!(runner.calls()[1].args[0], "/Query");

        // 3) 删除失败 + 任务仍在 → 报错
        let runner = MockRunner::new(vec![
            err_output("拒绝访问"),
            ok_output("\"\\glacc-auto-claim\",\"N/A\"\n"),
        ]);
        let err = unregister_with(&runner).unwrap_err();
        assert!(err.0.contains("拒绝访问"), "err={}", err.0);
    }

    /// 查询：定义可取 → 快照；确认不存在 → None；状态无法判定 → Err。
    #[test]
    fn query_maps_all_three_outcomes() {
        let xml = r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><CalendarTrigger>
    <StartBoundary>2026-10-17T08:30:00</StartBoundary>
    <ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay>
  </CalendarTrigger></Triggers>
  <Settings><StartWhenAvailable>false</StartWhenAvailable>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit></Settings>
  <Actions><Exec><Command>C:\Apps\glacc-auto.exe</Command>
    <Arguments>--scheduled</Arguments></Exec></Actions>
</Task>"#;
        let runner = MockRunner::always(ok_output(xml));
        let snap = query_snapshot_with(&runner).unwrap().unwrap();
        assert_eq!(snap.start_time, TimeOfDay::new(8, 30).unwrap());

        // 查询非零 + 列表确认不存在 → None
        let runner = MockRunner::new(vec![
            err_output("系统找不到指定的文件"),
            ok_output("\"\\Microsoft\\Windows\\Foo\",\"N/A\"\n"),
        ]);
        assert!(query_snapshot_with(&runner).unwrap().is_none());

        // 查询非零 + 列表说存在 → 无法取得定义
        let runner = MockRunner::new(vec![
            err_output("拒绝访问"),
            ok_output("\"\\glacc-auto-claim\",\"N/A\"\n"),
        ]);
        assert!(query_snapshot_with(&runner).is_err());

        // 查询非零 + 列表查询本身失败 → 状态无法判定
        let runner = MockRunner::new(vec![err_output("拒绝访问"), err_output("服务未启动")]);
        assert!(query_snapshot_with(&runner).is_err());
    }

    /// 测试入口：持门执行查询（与生产 query_snapshot 同路径，只是换了调用器）。
    fn query_snapshot_with(
        runner: &dyn SchtasksRunner,
    ) -> Result<Option<TaskSnapshot>, ScheduledTaskError> {
        let _guard = gate();
        query_locked(runner)
    }
}
