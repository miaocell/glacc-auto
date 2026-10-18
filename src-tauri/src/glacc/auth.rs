// 登录态管理（refresh / 短信登录），逐字移植自旧 Avalonia 版
// src\GlaccAuto.Core\Glacc\GlaccAuthClient.cs，对应更早的 _reverse/scripts/auth.py。
//
// 与旧版的对应关系：
// - 请求构造逐字对齐：五步 URL、body 字段名与顺序、header 集合与插入顺序（headerOrder 同源）。
// - 设备选择：登录流（captcha/发码/验码/signin）用 LoginDevice/LoginDeviceId + Auth 域 UA；
//   其余（refresh）用 Device/DeviceId。与旧版 PostJsonAsync 的 loginFlow 分支一致。
// - 落盘：pending_phone / verification_id 的保存都走 credentials.save() → store::write_atomic，
//   不另开文件（与旧版 _cred.Save() 同一落点）。
// - 注意：本链**不含任何请求签名**。旧版 captcha 是"雷盾 captcha_token 直签发"
//   （直签发 = 服务端直接签发，无人工挑战），请求体只有 action/client_id/device_id/meta/redirect_uri，
//   没有 sign 字段；sign.rs 的 push_sign 仅用于游戏域 mobileGLTaskPush。此处不做多余的签名拼接。
//
// 边界：本次不移植 session（重试/续期/旁路对账）、不接 UI、不发真实网络请求。
// 返回类型为 result.rs 的 GlaccResult（与旧版 GlaccResult 字段一一对应）。

use serde::Serialize;
use serde_json::Value;

use crate::glacc::constants;
use crate::glacc::credentials::Credentials;
use crate::glacc::device::{self, DeviceProfile};
use crate::glacc::result::GlaccResult;
use crate::glacc::store;
use crate::glacc::tls::{self, TlsError, TlsRequestPayload};

/// 本链路失败：need_relogin = false、retryable = false（旧版 GlaccResult.Fail 的默认值）。
fn fail(error: impl Into<String>) -> GlaccResult {
    GlaccResult::fail(error, false, false)
}

// ── 请求 DTO（字段名与顺序逐字对齐旧版 C# record 的 JsonPropertyName 声明序）──

#[derive(Debug, Serialize)]
struct TokenRefreshRequest {
    #[serde(rename = "grant_type")]
    grant_type: String,
    #[serde(rename = "refresh_token")]
    refresh_token: String,
    #[serde(rename = "client_id")]
    client_id: String,
    #[serde(rename = "client_secret")]
    client_secret: String,
}

#[derive(Debug, Serialize)]
struct CaptchaMeta {
    #[serde(rename = "phone_number")]
    phone_number: String,
}

#[derive(Debug, Serialize)]
struct CaptchaInitRequest {
    action: String,
    client_id: String,
    device_id: String,
    meta: CaptchaMeta,
    redirect_uri: String,
}

#[derive(Debug, Serialize)]
struct VerificationRequest {
    captcha_token: String,
    client_id: String,
    phone_number: String,
    target: String,
    usage: String,
}

#[derive(Debug, Serialize)]
struct VerifyCodeRequest {
    client_id: String,
    verification_id: String,
    verification_code: String,
}

#[derive(Debug, Serialize)]
struct SigninRequest {
    captcha_token: String,
    client_id: String,
    client_secret: String,
    username: String,
    verification_token: String,
}

// ── URL 构造（与旧版 $"{AuthBase}/..." 逐字一致）──

fn captcha_init_url() -> String {
    format!(
        "{}/v1/shield/captcha/init?client_id={}",
        constants::AUTH_BASE,
        constants::CLIENT_ID
    )
}

fn verification_url() -> String {
    format!("{}/v1/auth/verification", constants::AUTH_BASE)
}

fn verify_code_url() -> String {
    format!("{}/v1/auth/verification/verify", constants::AUTH_BASE)
}

fn signin_url() -> String {
    format!("{}/v1/auth/signin", constants::AUTH_BASE)
}

fn token_url() -> String {
    format!("{}/v1/auth/token", constants::AUTH_BASE)
}

// ── 请求头与载荷（顺序即语义：headerOrder 由它派生）──

/// 认证链路固定四个头，顺序与旧版 Dictionary 插入序一致。
fn auth_headers(device_id: &str, ua: &str) -> Vec<(String, String)> {
    vec![
        ("x-device-id".to_string(), device_id.to_string()),
        ("user-agent".to_string(), ua.to_string()),
        ("accept-language".to_string(), "zh-CN".to_string()),
        (
            "content-type".to_string(),
            "application/json; charset=utf-8".to_string(),
        ),
    ]
}

/// 组装 tls.rs 的 12 字段载荷：指纹随设备档案、头部保序、参数与旧版调用点逐字一致
/// （15 秒超时、跟随重定向、无 CookieJar、非字节请求/响应、catchPanics）。
fn build_payload(
    url: &str,
    body: String,
    device: &DeviceProfile,
    device_id: &str,
) -> TlsRequestPayload {
    let headers = auth_headers(device_id, &device::ua_auth(device));
    let header_order = Some(headers.iter().map(|(k, _)| k.clone()).collect());
    TlsRequestPayload {
        tls_client_identifier: device::identifier_for(device),
        request_method: "POST".to_string(),
        request_url: url.to_string(),
        request_body: Some(body),
        headers,
        header_order,
        timeout_seconds: 15,
        follow_redirects: true,
        without_cookie_jar: true,
        is_byte_request: false,
        is_byte_response: false,
        catch_panics: true,
    }
}

// ── 对外操作 ──

/// 发送短信验证码（captcha 直签发 + verification）。成功后 verification_id 存入凭证并落盘。
pub fn send_sms(cred: &mut Credentials, phone_input: &str) -> GlaccResult {
    let Some(phone) = normalize_phone(phone_input) else {
        return fail("手机号格式不正确（应为 11 位大陆手机号）");
    };

    // 设备标识与设备档案由手机号派生，captcha 请求起就要用：先定下当前登录目标。
    // 只记 pending_phone、不动 phone：登录成功才转正，改号登录期间不影响已登录会话的显示与身份
    cred.pending_phone = phone.clone();
    cred.save();

    let Some(captcha) = captcha_init(cred, "POST:/v1/auth/verification", &phone) else {
        store::log("发送验证码失败：未取得人机凭证");
        return fail("获取人机凭证失败，请稍后重试");
    };

    let resp = post_json(
        &verification_url(),
        &VerificationRequest {
            captcha_token: captcha,
            client_id: constants::CLIENT_ID.to_string(),
            phone_number: phone,
            target: "ANY".to_string(),
            usage: "SIGN_IN".to_string(),
        },
        cred,
        true,
    );
    let Some(body) = resp.body.as_deref() else {
        store::log(&format!("发送验证码失败（网络层）：{}", resp.error));
        return fail("网络错误，发送验证码失败");
    };
    let Some(root) = try_parse(body) else {
        store::log(&format!(
            "发送验证码失败：响应非 JSON（HTTP {}）",
            resp.status
        ));
        return fail("发送验证码失败：服务端响应异常");
    };

    let Some(vid) = root.get("verification_id") else {
        let msg = describe_error(&root);
        store::log(&format!("发送验证码失败：{msg}"));
        return fail(format!("发送验证码失败：{msg}"));
    };
    cred.verification_id = vid.as_str().unwrap_or_default().to_string();
    cred.verification_id_at = Credentials::now_seconds();
    cred.save();
    GlaccResult::success()
}

/// 用短信验证码完成登录（verify → captcha → signin），成功后凭证入库。
pub fn login(cred: &mut Credentials, code: &str) -> GlaccResult {
    let vid = cred.verification_id.clone();
    if vid.is_empty() {
        return fail("请先发送验证码");
    }
    if Credentials::now_seconds() - cred.verification_id_at > 280 {
        return fail("验证码已过期（5 分钟），请重新发送");
    }

    // 1) 验证码换 verification_token
    let v_resp = post_json(
        &verify_code_url(),
        &VerifyCodeRequest {
            client_id: constants::CLIENT_ID.to_string(),
            verification_id: vid,
            verification_code: code.to_string(),
        },
        cred,
        true,
    );
    let Some(v_body) = v_resp.body.as_deref() else {
        store::log(&format!("验证码校验失败（网络层）：{}", v_resp.error));
        return fail("网络错误，验证码校验失败");
    };
    let verification_token = {
        let Some(v_root) = try_parse(v_body) else {
            store::log(&format!(
                "验证码校验失败：响应非 JSON（HTTP {}）",
                v_resp.status
            ));
            return fail("验证码校验失败：服务端响应异常");
        };
        let Some(vt) = v_root.get("verification_token") else {
            let msg = describe_error(&v_root);
            store::log(&format!("验证码校验失败：{msg}"));
            return fail(format!("验证码校验失败：{msg}"));
        };
        vt.as_str().unwrap_or_default().to_string()
    };

    // 2) signin（signin action 需要自己的 captcha_token）
    let login_phone = cred.login_phone().to_string();
    let Some(captcha) = captcha_init(cred, "POST:/v1/auth/signin", &login_phone) else {
        store::log("登录失败：未取得人机凭证");
        return fail("获取人机凭证失败，请稍后重试");
    };

    let s_resp = post_json(
        &signin_url(),
        &SigninRequest {
            captcha_token: captcha,
            client_id: constants::CLIENT_ID.to_string(),
            client_secret: constants::CLIENT_SECRET.to_string(),
            username: login_phone.clone(),
            verification_token,
        },
        cred,
        true,
    );
    let Some(s_body) = s_resp.body.as_deref() else {
        store::log(&format!("登录失败（网络层）：{}", s_resp.error));
        return fail("网络错误，登录失败");
    };
    let Some(s_root) = try_parse(s_body) else {
        store::log(&format!("登录失败：响应非 JSON（HTTP {}）", s_resp.status));
        return fail("登录失败：服务端响应异常");
    };

    let Some(at) = s_root.get("access_token") else {
        let msg = describe_error(&s_root);
        store::log(&format!("登录失败：{msg}"));
        return fail(format!("登录失败：{msg}"));
    };

    cred.access_token = at.as_str().unwrap_or_default().to_string();
    if let Some(rt) = s_root.get("refresh_token") {
        cred.refresh_token = rt.as_str().unwrap_or_default().to_string();
    }
    if let Some(sub) = s_root.get("sub") {
        cred.sub = sub.as_str().unwrap_or_default().to_string();
    }
    cred.obtained_at = Credentials::now_seconds();
    cred.expires_in = s_root
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .unwrap_or(7200);
    // 登录完成：目标手机号转正，清理一次性状态
    cred.phone = login_phone;
    cred.pending_phone = String::new();
    cred.verification_id = String::new();
    cred.verification_id_at = 0;
    cred.save();
    GlaccResult::success()
}

/// 用 refresh_token 换新 JWT（端点 POST /v1/auth/token，JSON grant_type）。
/// 网络层失败（无响应/5xx/网关错误页）标记为可重试，由调用方按重试预算退避重试；
/// invalid_grant 是明确的业务判定，不重试。
pub fn refresh(cred: &mut Credentials) -> GlaccResult {
    let rt = cred.refresh_token.clone();
    if rt.is_empty() {
        store::log("刷新登录态失败：本地无 refresh_token");
        return fail("无 refresh_token，请重新登录");
    }

    let resp = post_json(
        &token_url(),
        &TokenRefreshRequest {
            grant_type: "refresh_token".to_string(),
            refresh_token: rt,
            client_id: constants::CLIENT_ID.to_string(),
            client_secret: constants::CLIENT_SECRET.to_string(),
        },
        cred,
        false,
    );
    if resp.transient() {
        store::log(&format!("刷新登录态失败（网络层）：{}", resp.error));
        return GlaccResult::fail("网络错误，刷新登录态失败", false, true);
    }
    let Some(body) = resp.body.as_deref() else {
        store::log("刷新登录态失败：无响应体");
        return GlaccResult::fail("网络错误，刷新登录态失败", false, true);
    };
    let Some(root) = try_parse(body) else {
        store::log(&format!(
            "刷新登录态失败：响应非 JSON（HTTP {}）",
            resp.status
        ));
        return GlaccResult::fail("刷新登录态失败：服务端响应异常", false, true);
    };

    let Some(at) = root.get("access_token") else {
        // invalid_grant / 4126 = refresh_token 已失效，需重新短信登录
        let invalid = root
            .get("error")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("invalid_grant"));
        store::log(&if invalid {
            "refresh_token 已失效，需重新短信登录".to_string()
        } else {
            format!("刷新登录态失败：{}", describe_error(&root))
        });
        return GlaccResult::fail(
            if invalid {
                "登录已过期，请重新短信登录".to_string()
            } else {
                format!("刷新登录态失败：{}", describe_error(&root))
            },
            invalid,
            false,
        );
    };
    cred.access_token = at.as_str().unwrap_or_default().to_string();
    if let Some(nrt) = root.get("refresh_token") {
        // 服务端可能轮换 refresh_token
        if let Some(s) = nrt.as_str() {
            cred.refresh_token = s.to_string();
        }
    }
    cred.obtained_at = Credentials::now_seconds();
    cred.expires_in = root
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .map(|v| v as i32)
        .unwrap_or(7200);
    cred.save();
    GlaccResult::success()
}

// ── 内部步骤 ──

/// 雷盾 captcha_token 直签发（无人工挑战，**不含请求签名**）；失败返回 None。
fn captcha_init(cred: &Credentials, action: &str, phone: &str) -> Option<String> {
    let resp = post_json(
        &captcha_init_url(),
        &CaptchaInitRequest {
            action: action.to_string(),
            client_id: constants::CLIENT_ID.to_string(),
            device_id: cred.login_device_id(),
            meta: CaptchaMeta {
                phone_number: phone.to_string(),
            },
            redirect_uri: constants::REDIRECT_URI.to_string(),
        },
        cred,
        true,
    );
    let body = resp.body?;
    let root = try_parse(&body)?;
    root.get("captcha_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// 一次 POST 的原始结果：body 为 None 表示未取得响应（网络层失败）。
struct AuthHttpResponse {
    body: Option<String>,
    status: i32,
    error: String,
}

impl AuthHttpResponse {
    /// 未取得响应或服务端 5xx：网络层失败，未产生业务判定。
    fn transient(&self) -> bool {
        self.body.is_none() || self.status >= 500
    }
}

/// 认证链路的请求主体（走 tls.rs：OkHttp/Android TLS 指纹 + 设备档案 UA）。
/// 短信登录流程（发码/验码/signin）传 login_flow = true：设备身份跟随待登录手机号；
/// 其余（refresh）跟随已登录账号，改号登录进行中不影响既有会话的请求身份。
/// 注意：本函数是同步阻塞调用（tls::send 阻塞），调用方须放在后台线程。
fn post_json<T: Serialize>(
    url: &str,
    payload: &T,
    cred: &Credentials,
    login_flow: bool,
) -> AuthHttpResponse {
    let device = if login_flow {
        cred.login_device()
    } else {
        cred.device()
    };
    let device_id = if login_flow {
        cred.login_device_id()
    } else {
        cred.device_id()
    };
    // 与旧版一致：body 为该上下文下的 WriteIndented 风格（Go 侧按 JSON 解析，缩进不影响语义）
    let body = match serde_json::to_string_pretty(payload) {
        Ok(b) => b,
        Err(e) => {
            return AuthHttpResponse {
                body: None,
                status: 0,
                error: format!("请求序列化失败：{e}"),
            };
        }
    };
    let tls_payload = build_payload(url, body, &device, &device_id);
    match tls::send(&tls_payload) {
        Ok(resp) => AuthHttpResponse {
            body: Some(resp.body),
            status: resp.status,
            error: String::new(),
        },
        Err(TlsError::Unavailable(msg)) => {
            // 本机指纹库故障：与旧版一致，落日志后按"未取得响应"返回
            store::log(&format!("TLS 指纹库不可用（{}）", endpoint(url)));
            AuthHttpResponse {
                body: None,
                status: 0,
                error: msg,
            }
        }
        Err(TlsError::Request(msg)) => {
            store::log(&format!("认证接口无响应（{}）：{msg}", endpoint(url)));
            AuthHttpResponse {
                body: None,
                status: 0,
                error: msg,
            }
        }
    }
}

/// 日志用端点名：只保留路径，不带查询参数（对应旧版 Uri.AbsolutePath）。
fn endpoint(url: &str) -> String {
    if let Some(pos) = url.find("://") {
        let after = &url[pos + 3..];
        return match after.find('/') {
            Some(slash) => {
                let path = &after[slash..];
                let end = path.find(['?', '#']).unwrap_or(path.len());
                path[..end].to_string()
            }
            None => "/".to_string(),
        };
    }
    url.to_string()
}

/// 解析响应体；非 JSON（如网关错误页）返回 None，避免异常外泄到业务层。
fn try_parse(body: &str) -> Option<Value> {
    if body.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(body).ok()
}

/// 尽量提取 error / error_description / message / msg / code（提取顺序与文案逐字对齐旧版）。
fn describe_error(root: &Value) -> String {
    let Some(obj) = root.as_object() else {
        let raw = serde_json::to_string(root).unwrap_or_default();
        return if !raw.is_empty() {
            raw
        } else {
            "未知错误".to_string()
        };
    };
    for key in ["error_description", "error", "message", "msg"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    if let Some(code) = obj.get("code") {
        return format!("code={}", serde_json::to_string(code).unwrap_or_default());
    }
    "未知错误".to_string()
}

/// 11 位手机号 → "+86 12xxxxxxxxx"（与官方客户端一致的格式）；不合法返回 None。
pub fn normalize_phone(input: &str) -> Option<String> {
    let mut digits: String = input.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() == 13 && digits.starts_with("86") {
        digits = digits[2..].to_string();
    }
    if digits.len() == 11 && digits.starts_with('1') {
        Some(format!("+86 {digits}"))
    } else {
        None
    }
}

// 说明：本链不涉及请求签名（见文件头注释），不需要 sign.rs。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glacc::store::ENV_GUARD;

    const SALT: &str = "0123456789abcdef0123456789abcdef";
    const PHONE: &str = "13800138000";

    fn cred() -> Credentials {
        Credentials {
            install_salt: SALT.to_string(),
            ..Credentials::default()
        }
    }

    /// NormalizePhone：五种输入写法逐一断言（与旧版 NormalizePhone 逐字一致）。
    #[test]
    fn normalize_phone_matches_legacy() {
        // 1 纯 11 位数字
        assert_eq!(
            normalize_phone("13800138000"),
            Some("+86 13800138000".to_string())
        );
        // 2 带 86 前缀（13 位数字，剥前缀）
        assert_eq!(
            normalize_phone("8613800138000"),
            Some("+86 13800138000".to_string())
        );
        // 3 带加号与 86 前缀
        assert_eq!(
            normalize_phone("+8613800138000"),
            Some("+86 13800138000".to_string())
        );
        // 4 带空格（去非数字后再归一）
        assert_eq!(
            normalize_phone("138 0013 8000"),
            Some("+86 13800138000".to_string())
        );
        // 5 不合法：长度不足 / 空串 / 11 位但不以 1 开头
        assert_eq!(normalize_phone("1380013"), None);
        assert_eq!(normalize_phone(""), None);
        assert_eq!(normalize_phone("23800138000"), None);
    }

    /// 五步请求构造：URL、query、body 字段名与顺序、header 顺序逐项与旧版对齐。
    #[test]
    fn five_step_request_construction_matches_legacy() {
        // 1 captcha 直签（发码用）：URL 带 query，body 五字段
        assert_eq!(
            captcha_init_url(),
            "https://user.geilijiasu.net/v1/shield/captcha/init?client_id=abjIHOBRMk89gOKc"
        );
        let body = serde_json::to_string(&CaptchaInitRequest {
            action: "POST:/v1/auth/verification".to_string(),
            client_id: constants::CLIENT_ID.to_string(),
            device_id: "DEV".to_string(),
            meta: CaptchaMeta {
                phone_number: "+86 13800138000".to_string(),
            },
            redirect_uri: constants::REDIRECT_URI.to_string(),
        })
        .unwrap();
        assert_eq!(
            body,
            r#"{"action":"POST:/v1/auth/verification","client_id":"abjIHOBRMk89gOKc","device_id":"DEV","meta":{"phone_number":"+86 13800138000"},"redirect_uri":"xlaccsdk01://xunlei.com/callback?state=harbor"}"#
        );

        // 2 发码：body 五字段，target=ANY、usage=SIGN_IN
        assert_eq!(
            verification_url(),
            "https://user.geilijiasu.net/v1/auth/verification"
        );
        let body = serde_json::to_string(&VerificationRequest {
            captcha_token: "CT".to_string(),
            client_id: constants::CLIENT_ID.to_string(),
            phone_number: "+86 13800138000".to_string(),
            target: "ANY".to_string(),
            usage: "SIGN_IN".to_string(),
        })
        .unwrap();
        assert_eq!(
            body,
            r#"{"captcha_token":"CT","client_id":"abjIHOBRMk89gOKc","phone_number":"+86 13800138000","target":"ANY","usage":"SIGN_IN"}"#
        );

        // 3 验码：body 三字段
        assert_eq!(
            verify_code_url(),
            "https://user.geilijiasu.net/v1/auth/verification/verify"
        );
        let body = serde_json::to_string(&VerifyCodeRequest {
            client_id: constants::CLIENT_ID.to_string(),
            verification_id: "VID".to_string(),
            verification_code: "123456".to_string(),
        })
        .unwrap();
        assert_eq!(
            body,
            r#"{"client_id":"abjIHOBRMk89gOKc","verification_id":"VID","verification_code":"123456"}"#
        );

        // 4 signin：body 五字段（含 client_secret 与 username）
        assert_eq!(signin_url(), "https://user.geilijiasu.net/v1/auth/signin");
        let body = serde_json::to_string(&SigninRequest {
            captcha_token: "CT2".to_string(),
            client_id: constants::CLIENT_ID.to_string(),
            client_secret: constants::CLIENT_SECRET.to_string(),
            username: "+86 13800138000".to_string(),
            verification_token: "VT".to_string(),
        })
        .unwrap();
        assert_eq!(
            body,
            r#"{"captcha_token":"CT2","client_id":"abjIHOBRMk89gOKc","client_secret":"BihWgSGwEfGC_IKQXAAUlQ","username":"+86 13800138000","verification_token":"VT"}"#
        );

        // 5 refresh：body 四字段，grant_type=refresh_token
        assert_eq!(token_url(), "https://user.geilijiasu.net/v1/auth/token");
        let body = serde_json::to_string(&TokenRefreshRequest {
            grant_type: "refresh_token".to_string(),
            refresh_token: "RT".to_string(),
            client_id: constants::CLIENT_ID.to_string(),
            client_secret: constants::CLIENT_SECRET.to_string(),
        })
        .unwrap();
        assert_eq!(
            body,
            r#"{"grant_type":"refresh_token","refresh_token":"RT","client_id":"abjIHOBRMk89gOKc","client_secret":"BihWgSGwEfGC_IKQXAAUlQ"}"#
        );
    }

    /// 设备选择与 header 顺序：登录流用 LoginDevice/LoginDeviceId，refresh 用 Device/DeviceId；
    /// 头部顺序固定为 x-device-id → user-agent → accept-language → content-type。
    #[test]
    fn device_selection_and_header_order() {
        let mut c = cred();
        c.phone = PHONE.to_string();
        c.pending_phone = "15212345678".to_string();

        // 登录流：device/UA 跟随 pending 号
        let login_device = c.login_device();
        let tls_payload = build_payload(
            &verification_url(),
            "{}".to_string(),
            &login_device,
            &c.login_device_id(),
        );
        assert_eq!(
            tls_payload.tls_client_identifier,
            device::identifier_for(&login_device)
        );
        assert_eq!(
            tls_payload.tls_client_identifier, "okhttp4_android_13",
            "15212345678 → idx8/Android13"
        );
        let order: Vec<String> = tls_payload.headers.iter().map(|(k, _)| k.clone()).collect();
        assert_eq!(
            order,
            vec![
                "x-device-id",
                "user-agent",
                "accept-language",
                "content-type"
            ]
        );
        assert_eq!(tls_payload.header_order.as_ref().unwrap(), &order);
        // UA 为 Auth 域模板，且带 pending 号选出的档案
        let ua = &tls_payload.headers[1].1;
        assert!(ua.starts_with("ANDROID-com.geilijiasu.glacc/1.26.8.2 netWorkType/WIFI"));
        assert!(ua.contains("deviceName/Redmi_23054RA19C"), "UA={ua}");
        // x-device-id 跟随 pending 号派生的登录域设备 ID
        assert_eq!(
            tls_payload.headers[0].1,
            c.login_device_id(),
            "登录流须用 LoginDeviceId"
        );

        // 非登录流（refresh）：device/UA 跟随已登录号，头部顺序不变
        let device = c.device();
        let tls_payload = build_payload(&token_url(), "{}".to_string(), &device, &c.device_id());
        assert_eq!(tls_payload.headers[0].1, c.device_id());
        assert!(
            tls_payload.headers[1]
                .1
                .contains("deviceName/Redmi_2210132C"),
            "13800138000 → idx1"
        );
        // 12 字段形态（与 tls.rs 一致）
        let v: Value = serde_json::to_value(&tls_payload).unwrap();
        assert_eq!(v.as_object().unwrap().len(), 12);
        assert_eq!(v["requestMethod"], "POST");
        assert_eq!(v["timeoutSeconds"], 15);
        assert_eq!(v["followRedirects"], true);
        assert_eq!(v["withoutCookieJar"], true);
        assert_eq!(v["isByteRequest"], false);
        assert_eq!(v["isByteResponse"], false);
        assert_eq!(v["catchPanics"], true);
    }

    /// DescribeError：提取顺序（error_description → error → message → msg → code）与文案。
    #[test]
    fn describe_error_matches_legacy() {
        let j = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!(
            describe_error(&j(r#"{"error_description":"描述优先"}"#)),
            "描述优先"
        );
        assert_eq!(
            describe_error(&j(r#"{"error":"invalid_grant"}"#)),
            "invalid_grant"
        );
        assert_eq!(describe_error(&j(r#"{"message":"m"}"#)), "m");
        assert_eq!(describe_error(&j(r#"{"msg":"g"}"#)), "g");
        // 优先级：error_description 优先于 error
        assert_eq!(
            describe_error(&j(r#"{"error":"e","error_description":"d"}"#)),
            "d"
        );
        // 非字符串值跳过；全都不匹配时看 code
        assert_eq!(
            describe_error(&j(r#"{"error":123,"code":10003}"#)),
            "code=10003"
        );
        assert_eq!(describe_error(&j(r#"{"code":"ABC"}"#)), "code=\"ABC\"");
        // 空字符串视为未命中
        assert_eq!(describe_error(&j(r#"{"error":"","message":"m2"}"#)), "m2");
        // 无任何可提取字段
        assert_eq!(describe_error(&j("{}")), "未知错误");
        // 非对象：返回原始文本
        assert_eq!(describe_error(&j("123")), "123");
        assert_eq!(describe_error(&j(r#""str""#)), "\"str\"");
    }

    /// Transient：无响应 → true；5xx → true；4xx/2xx → false。
    #[test]
    fn transient_rules_match_legacy() {
        let mk = |body: Option<&str>, status: i32| AuthHttpResponse {
            body: body.map(|s| s.to_string()),
            status,
            error: String::new(),
        };
        // 无响应（网络层失败）
        assert!(mk(None, 0).transient());
        // 5xx（含带 body 的 5xx）
        assert!(mk(None, 500).transient());
        assert!(mk(Some("{\"code\":0}"), 500).transient());
        assert!(mk(Some("gateway error"), 502).transient());
        // 4xx：取得响应即非 transient（auth 链对 4xx 有自己的解析路径）
        assert!(!mk(Some("{}"), 400).transient());
        assert!(!mk(Some("{}"), 401).transient());
        assert!(!mk(Some("{}"), 403).transient());
        // 2xx
        assert!(!mk(Some("{}"), 200).transient());
    }

    /// 验码有效期：280 秒内可继续、超过即拒绝（不触网）。
    #[test]
    fn verification_id_expiry_is_checked_before_network() {
        let mut c = cred();
        c.phone = PHONE.to_string();
        c.pending_phone = PHONE.to_string();
        // 无 verification_id：直接拒绝
        let r = login(&mut c, "123456");
        assert!(!r.ok);
        assert_eq!(r.error, "请先发送验证码");

        // 超过 280 秒：拒绝（时间戳设在过去）
        c.verification_id = "VID".to_string();
        c.verification_id_at = Credentials::now_seconds() - 281;
        let r = login(&mut c, "123456");
        assert!(!r.ok);
        assert_eq!(r.error, "验证码已过期（5 分钟），请重新发送");

        // 未超期：会进入网络步骤 —— 无 dll/离线环境下应返回网络错误而非 panic，
        // 但这属于"真实网络调用"路径，按红线不在此用例执行（见 ignored 手动用例）。
    }

    /// Endpoint：只保留路径，不带 query（对应旧版 Uri.AbsolutePath）。
    #[test]
    fn endpoint_extracts_path_only() {
        assert_eq!(
            endpoint("https://user.geilijiasu.net/v1/shield/captcha/init?client_id=abc"),
            "/v1/shield/captcha/init"
        );
        assert_eq!(
            endpoint("https://user.geilijiasu.net/v1/auth/verification"),
            "/v1/auth/verification"
        );
        assert_eq!(endpoint("https://user.geilijiasu.net"), "/");
        assert_eq!(endpoint("not a url"), "not a url");
    }

    /// refresh 前置校验（不触网）：无 refresh_token 直接拒绝。
    #[test]
    fn refresh_requires_local_token() {
        let mut c = cred();
        let r = refresh(&mut c);
        assert!(!r.ok);
        assert!(!r.need_relogin);
        assert!(!r.retryable);
        assert_eq!(r.error, "无 refresh_token，请重新登录");
    }

    /// 发码前置校验（不触网）：号码不合法直接拒绝，且不改动 pending_phone。
    #[test]
    fn send_sms_rejects_invalid_phone_without_network() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("glacc-tauri-step1cc-auth");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("GLACC_AUTO_DATA", &dir);

        let mut c = cred();
        let r = send_sms(&mut c, "1380013");
        assert!(!r.ok);
        assert_eq!(r.error, "手机号格式不正确（应为 11 位大陆手机号）");
        assert_eq!(c.pending_phone, "", "不合法号码不应写入 pending_phone");

        std::env::remove_var("GLACC_AUTO_DATA");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 手动网络用例（默认 ignored，需 miao 授权后手动跑；会发真实短信/需要真实账号）──

    /// 手动：真实发送短信验证码。运行前设置 GLACC_AUTH_TEST_PHONE，且需 miao 明确授权。
    /// 例：GLACC_AUTH_TEST_PHONE=13800138000 cargo test --lib glacc::auth -- --ignored --nocapture
    #[test]
    #[ignore = "需真实短信，miao 授权后手动跑"]
    fn manual_send_sms() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("glacc-tauri-step1cc-manual-sms");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("GLACC_AUTO_DATA", &dir);

        let phone = std::env::var("GLACC_AUTH_TEST_PHONE")
            .expect("需先设置 GLACC_AUTH_TEST_PHONE（真实手机号）");
        let mut c = cred();
        c.ensure_install_salt();
        let r = send_sms(&mut c, &phone);
        println!(
            "send_sms ok={} error={} pending={} vid_len={}",
            r.ok,
            r.error,
            c.pending_phone,
            c.verification_id.len()
        );
        std::env::remove_var("GLACC_AUTO_DATA");
    }

    /// 手动：真实登录（先发码、再带验证码执行 login）。运行前设置 GLACC_AUTH_TEST_PHONE
    /// 与 GLACC_AUTH_TEST_CODE（用户收到的验证码），且需 miao 明确授权。
    #[test]
    #[ignore = "需真实短信验证码，miao 授权后手动跑"]
    fn manual_login_with_code() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("glacc-tauri-step1cc-manual-login");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("GLACC_AUTO_DATA", &dir);

        let phone = std::env::var("GLACC_AUTH_TEST_PHONE")
            .expect("需先设置 GLACC_AUTH_TEST_PHONE（真实手机号）");
        let code = std::env::var("GLACC_AUTH_TEST_CODE").expect("需设置 GLACC_AUTH_TEST_CODE");
        let mut c = cred();
        c.ensure_install_salt();
        let sms = send_sms(&mut c, &phone);
        assert!(sms.ok, "发码失败：{}", sms.error);
        let r = login(&mut c, &code);
        println!(
            "login ok={} error={} phone={} has_token={}",
            r.ok,
            r.error,
            c.phone,
            c.has_token()
        );
        std::env::remove_var("GLACC_AUTO_DATA");
    }

    /// 手动：真实刷新登录态。运行前设置 GLACC_AUTH_TEST_REFRESH_TOKEN，且需 miao 明确授权。
    #[test]
    #[ignore = "需真实 refresh_token，miao 授权后手动跑"]
    fn manual_refresh() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("glacc-tauri-step1cc-manual-refresh");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("GLACC_AUTO_DATA", &dir);

        let rt = std::env::var("GLACC_AUTH_TEST_REFRESH_TOKEN")
            .expect("需先设置 GLACC_AUTH_TEST_REFRESH_TOKEN");
        let mut c = cred();
        c.install_salt = SALT.to_string();
        c.phone = PHONE.to_string();
        c.refresh_token = rt;
        let r = refresh(&mut c);
        println!(
            "refresh ok={} error={} need_relogin={} retryable={} has_token={}",
            r.ok,
            r.error,
            r.need_relogin,
            r.retryable,
            c.has_token()
        );
        std::env::remove_var("GLACC_AUTO_DATA");
    }
}
