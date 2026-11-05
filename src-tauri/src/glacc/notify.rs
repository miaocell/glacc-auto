// Server 酱 Turbo 推送客户端，移植自旧 Avalonia 版
// src\GlaccAuto.Core\Notify\ServerChanClient.cs。
//
// 旧版核实结论：
// - 仅定时自动运行的收尾发送结果通知；发送失败一律静默返回 false，不影响领取主流程与退出逻辑
// - SendKey 校验：非空且匹配 ^SCT[0-9A-Za-z]+$；空即视为未启用
// - URL：https://sctapi.ftqq.com/{sendKey}.send
// - 方法：POST，body 为 form-urlencoded（title、desp 两个字段）
// - 响应判定：HTTP 2xx 且 JSON 对象里 code 为整数 0
// - title 超过 32 字符截断；desp 支持 Markdown
//
// 与旧版的唯一差异（须留意）：旧版用 .NET HttpClient，走系统 TLS 栈；任务书要求改用 tls.rs 的
// send 封装（OkHttp/Android 指纹）。这里是"通知域"请求，不带任何游戏域头部与凭据，仅借用同一套
// TLS 栈以复用零依赖的 HTTP 能力；对 Server 酱而言 UA/指纹不影响功能。

use std::sync::Arc;

use crate::glacc::diag;
use crate::glacc::tls::{self, TlsError, TlsRequestPayload, TlsResponse};

/// Server 酱 Turbo 接口域名。
pub const API_BASE: &str = "https://sctapi.ftqq.com";
/// title 最大长度（旧版硬截断）。
pub const TITLE_MAX: usize = 32;

/// HTTP 传输层（测试注入 Mock，绝不触网）。
pub type Transport = Arc<dyn Fn(&TlsRequestPayload) -> Result<TlsResponse, TlsError> + Send + Sync>;

/// 默认传输：走 tls-client 原生库。
pub fn native_transport() -> Transport {
    Arc::new(tls::send)
}

/// SendKey 是否有效（非空且为 SCT 开头的字母数字串）；空即视为未启用。
pub fn is_configured(send_key: &str) -> bool {
    let k = send_key.trim();
    let mut chars = k.chars();
    match chars.next() {
        Some('S') => {}
        _ => return false,
    }
    match chars.next() {
        Some('C') => {}
        _ => return false,
    }
    match chars.next() {
        Some('T') => {}
        _ => return false,
    }
    let rest: String = chars.collect();
    !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric())
}

/// 目标 URL（便于测试断言与日志）。
pub fn endpoint(send_key: &str) -> String {
    format!("{API_BASE}/{}.send", send_key.trim())
}

/// form-urlencoded 编码（等价 .NET FormUrlEncodedContent：空格转 '+'，其余按 RFC 3986 百分号编码，
/// 不编码 A-Za-z0-9-._*）。
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    fn encode_component(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode_component(k), encode_component(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// 构造推送请求载荷。
pub fn build_payload(send_key: &str, title: &str, desp: &str) -> TlsRequestPayload {
    let title = truncate_chars(title, TITLE_MAX);
    let body = form_encode(&[("title", &title), ("desp", desp)]);
    let headers: Vec<(String, String)> = vec![
        (
            "content-type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        ),
        ("user-agent".to_string(), "glacc-auto".to_string()),
    ];
    TlsRequestPayload {
        tls_client_identifier: "okhttp4_android_13".to_string(),
        request_method: "POST".to_string(),
        request_url: endpoint(send_key),
        request_body: Some(body),
        header_order: Some(headers.iter().map(|(k, _)| k.clone()).collect()),
        headers,
        timeout_seconds: 10,
        follow_redirects: true,
        without_cookie_jar: true,
        is_byte_request: false,
        is_byte_response: false,
        catch_panics: true,
    }
}

/// 按字符截断（旧版 title[..32] 按 UTF-16 码元切，中文不越界；此处按 char 切，语义一致）。
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// 推送结果：未配置时返回 Skipped，不视为失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// 已发送且服务端 code=0
    Sent,
    /// 配置未启用或缺 SendKey：静默跳过，零请求
    Skipped,
    /// 已尝试但失败（HTTP 非 2xx / code != 0 / 传输异常），仅落日志
    Failed,
}

/// 解析响应：HTTP 2xx 且 JSON 对象中 code 为整数 0 才算成功。
fn response_ok(resp: &TlsResponse) -> bool {
    if !(200..300).contains(&resp.status) {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&resp.body) else {
        return false;
    };
    v.as_object().is_some()
        && v.get("code")
            .and_then(|c| c.as_i64())
            .is_some_and(|code| code == 0)
}

/// 发送一条消息；title 超过 32 字符自动截断，desp 支持 Markdown。
/// 未配置 → Skipped（零请求）；任何失败 → Failed 且只落日志，不 panic、不向上抛。
pub fn send_with(
    transport: &Transport,
    send_key: &str,
    title: &str,
    desp: &str,
) -> NotifyOutcome {
    if !is_configured(send_key) {
        diag::info("未配置 Server酱通知，跳过结果推送");
        return NotifyOutcome::Skipped;
    }
    let payload = build_payload(send_key, title, desp);
    match transport(&payload) {
        Ok(resp) if response_ok(&resp) => {
            diag::info("定时领取结果通知已发送");
            NotifyOutcome::Sent
        }
        Ok(resp) => {
            diag::warn(&format!("Server酱通知发送失败：HTTP {}", resp.status));
            NotifyOutcome::Failed
        }
        Err(e) => {
            diag::warn(&format!("Server酱通知发送异常：{e}"));
            NotifyOutcome::Failed
        }
    }
}

/// 用默认传输发送（生产入口）。
pub fn send(send_key: &str, title: &str, desp: &str) -> NotifyOutcome {
    let transport = native_transport();
    send_with(&transport, send_key, title, desp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // 通知测试不落真实日志文件：diag::warn 会按 data_root 写盘，
    // 故这里用 GLACC_AUTO_DATA 指向临时目录（与 store 的环境变量锁串行）。
    struct Isolated {
        dir: std::path::PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Isolated {
        fn new(tag: &str) -> Isolated {
            let lock = crate::glacc::store::ENV_GUARD
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("glacc-tauri-step2c-notify-{tag}"));
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

    /// SendKey 校验：SCT 前缀 + 至少一个字母数字，其余一律无效。
    #[test]
    fn send_key_validation() {
        assert!(is_configured("SCT123456Tabcdef"));
        assert!(is_configured("  SCTabc  "), "首尾空白应被 trim");
        assert!(is_configured("SCT1"));
        assert!(!is_configured(""));
        assert!(!is_configured("   "));
        assert!(!is_configured("SCT"));
        assert!(!is_configured("sct123"), "前缀区分大小写");
        assert!(!is_configured("ABC123"));
        assert!(!is_configured("SCT12-34"), "不允许连字符");
        assert!(!is_configured("SCT12 34"), "不允许空格");
    }

    /// 未配置时零请求发出（transport 若被调用即 panic）。
    #[test]
    fn not_configured_sends_nothing() {
        let _iso = Isolated::new("unconfigured");
        let called = Arc::new(Mutex::new(0));
        let c = called.clone();
        let transport: Transport = Arc::new(move |_p: &TlsRequestPayload| {
            *c.lock().unwrap() += 1;
            panic!("未配置时不得发出任何请求");
        });
        assert_eq!(
            send_with(&transport, "", "t", "d"),
            NotifyOutcome::Skipped
        );
        assert_eq!(
            send_with(&transport, "   ", "t", "d"),
            NotifyOutcome::Skipped
        );
        assert_eq!(send_with(&transport, "bad-key", "t", "d"), NotifyOutcome::Skipped);
        assert_eq!(*called.lock().unwrap(), 0, "零请求");
    }

    /// URL 拼接与 body 格式（form-urlencoded，title/desp 两字段，中文百分号编码）。
    #[test]
    fn payload_url_and_body() {
        let p = build_payload("SCT123abc", "定时领取成功", "**完成**\n- 时间: 2026-10-17 08:00:00");
        assert_eq!(p.request_url, "https://sctapi.ftqq.com/SCT123abc.send");
        assert_eq!(p.request_method, "POST");
        assert_eq!(p.timeout_seconds, 10);
        assert!(!p.is_byte_request && !p.is_byte_response && p.catch_panics);

        let body = p.request_body.as_ref().unwrap();
        assert!(body.starts_with("title="), "body={body}");
        assert!(body.contains("&desp="), "body={body}");
        // 中文按 UTF-8 字节百分号编码；不应出现原字符
        assert!(!body.contains("定时"), "body={body}");
        assert!(body.contains("%E5%AE%9A%E6%97%B6"), "body={body}");
        // 空格编码为 '+'（FormUrlEncodedContent 口径）
        let with_space = build_payload("SCT1", "a b", "c d");
        assert_eq!(
            with_space.request_body.as_deref(),
            Some("title=a+b&desp=c+d")
        );
        // 换行编码为 %0A
        assert!(body.contains("%0A"), "body={body}");

        let ct = p
            .headers
            .iter()
            .find(|(k, _)| k == "content-type")
            .map(|(_, v)| v.as_str());
        assert_eq!(ct, Some("application/x-www-form-urlencoded"));
        let order: Vec<&str> = p.header_order.as_ref().unwrap().iter().map(|s| s.as_str()).collect();
        assert_eq!(order, vec!["content-type", "user-agent"]);
    }

    /// title 超过 32 字符截断（中文按字符计，不产生半个字）。
    #[test]
    fn title_is_truncated_to_32_chars() {
        let long: String = "字".repeat(40);
        let p = build_payload("SCT1", &long, "d");
        let body = p.request_body.as_ref().unwrap();
        let title_part = body.split("&desp=").next().unwrap();
        let encoded = title_part.strip_prefix("title=").unwrap();
        let decoded = percent_decode(encoded);
        assert_eq!(decoded.chars().count(), TITLE_MAX);
        assert!(decoded.chars().all(|c| c == '字'));
    }

    /// 成功判定：2xx + code=0；其余情形均 Failed 且不 panic。
    #[test]
    fn response_judgement_and_failures() {
        let _iso = Isolated::new("responses");

        let ok = |body: &str, status: i32| {
            let b = body.to_string();
            let transport: Transport =
                Arc::new(move |_p: &TlsRequestPayload| {
                    Ok(TlsResponse {
                        status,
                        body: b.clone(),
                    })
                });
            send_with(&transport, "SCT1", "t", "d")
        };

        assert_eq!(ok(r#"{"code":0,"message":""}"#, 200), NotifyOutcome::Sent);
        assert_eq!(ok(r#"{"code":40001}"#, 200), NotifyOutcome::Failed);
        assert_eq!(ok(r#"{"code":"0"}"#, 200), NotifyOutcome::Failed, "字符串 code 不算成功");
        assert_eq!(ok(r#"{"message":"x"}"#, 200), NotifyOutcome::Failed, "缺 code");
        assert_eq!(ok("<html>gateway</html>", 200), NotifyOutcome::Failed);
        assert_eq!(ok(r#"{"code":0}"#, 500), NotifyOutcome::Failed, "5xx 不算成功");
        assert_eq!(ok(r#"{"code":0}"#, 302), NotifyOutcome::Failed, "非 2xx 不算成功");

        // 传输异常：只记日志、返回 Failed，不 panic
        let err_transport: Transport =
            Arc::new(|_p: &TlsRequestPayload| Err(TlsError::Request("网络不可达".to_string())));
        assert_eq!(
            send_with(&err_transport, "SCT1", "t", "d"),
            NotifyOutcome::Failed
        );

        // 发送成功/失败都落日志，且不因日志写盘失败而改变返回值
        let text = std::fs::read_to_string(crate::glacc::diag::current_file_path()).unwrap();
        assert!(text.contains("通知已发送"), "text={text}");
        assert!(text.contains("Server酱通知发送失败"), "text={text}");
    }

    /// 最小 percent-decode（仅测试用：解 '+' 与 %XX）。
    fn percent_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b'%' if i + 2 < bytes.len() => {
                    let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
                    out.push(u8::from_str_radix(hex, 16).unwrap());
                    i += 3;
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8(out).unwrap()
    }
}
