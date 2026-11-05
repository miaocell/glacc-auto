// 结果类型，逐字移植自旧 Avalonia 版：
// - src\GlaccAuto.Core\Glacc\GlaccResult.cs（GlaccResult）
// - src\GlaccAuto.Core\Glacc\GlaccSession.cs（GlaccFailReason / GlaccHttpResponse / GlaccCallResult<T>）
//
// 说明：GlaccResult.cs 里还含 GlaccTaskStage 与 GlaccPushResult，两者是游戏域数据结构
// （来自 mobileGLTaskList / mobileGLTaskPush），按本次边界不落入 result.rs，留待 game 域步骤。

use crate::glacc::tls::TlsResponse;

/// 操作结果（带用户可读错误信息）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GlaccResult {
    pub ok: bool,
    /// true = refresh_token 已失效（invalid_grant/4126），需重新短信登录
    pub need_relogin: bool,
    /// true = 网络层失败且自动重试是安全的（未取得业务判定，重复执行无副作用）。
    /// 仅刷新登录态会置位：发短信/登录/校验验证码重试会产生重复请求，一律不置位。
    pub retryable: bool,
    pub error: String,
}

impl GlaccResult {
    pub fn success() -> Self {
        Self {
            ok: true,
            need_relogin: false,
            retryable: false,
            error: String::new(),
        }
    }

    /// 等价旧版 GlaccResult.Fail(error, needRelogin = false, retryable = false)。
    pub fn fail(error: impl Into<String>, need_relogin: bool, retryable: bool) -> Self {
        Self {
            ok: false,
            need_relogin,
            retryable,
            error: error.into(),
        }
    }
}

/// 业务请求失败原因（与旧版枚举逐值对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlaccFailReason {
    /// 请求成功
    None,
    /// 未取得响应：网络不可达、超时
    NoResponse,
    /// 服务端 5xx：服务端临时故障
    ServerError,
    /// 取得响应但无法解析或不符合预期：接口可能已变更
    BadResponse,
    /// 客户端自身故障：TLS 指纹库缺失或无法加载
    ClientError,
    /// 登录态无法续期（refresh_token 失效），需重新短信登录
    NeedRelogin,
    /// 本地无登录凭证：尚未登录，需先完成短信登录
    NotLoggedIn,
}

/// 一次请求的传输结果：response 为 None 表示未取得响应，
/// 此时 error 给出技术细节（异常类型与信息），供日志与诊断展示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlaccHttpResponse {
    pub response: Option<TlsResponse>,
    pub error: String,
    pub reason: GlaccFailReason,
}

impl GlaccHttpResponse {
    pub fn new(
        response: Option<TlsResponse>,
        error: impl Into<String>,
        reason: GlaccFailReason,
    ) -> Self {
        Self {
            response,
            error: error.into(),
            reason,
        }
    }

    /// 未取得响应（默认 NoResponse）。
    pub fn no_response(error: impl Into<String>) -> Self {
        Self::new(None, error, GlaccFailReason::NoResponse)
    }

    /// TLS 指纹库不可用（与网络故障区分）。
    pub fn from_client_fault(error: impl Into<String>) -> Self {
        Self::new(None, error, GlaccFailReason::ClientError)
    }
}

/// 统一请求结果：value 仅在 ok() 时有效；detail 为技术细节，不作为面向用户的主提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlaccCallResult<T> {
    pub value: Option<T>,
    pub reason: GlaccFailReason,
    pub detail: String,
}

impl<T> GlaccCallResult<T> {
    pub fn ok(&self) -> bool {
        self.reason == GlaccFailReason::None
    }

    pub fn success(value: T) -> Self {
        Self {
            value: Some(value),
            reason: GlaccFailReason::None,
            detail: String::new(),
        }
    }

    pub fn fail(reason: GlaccFailReason, detail: impl Into<String>) -> Self {
        Self {
            value: None,
            reason,
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_constructors_match_legacy() {
        let s = GlaccResult::success();
        assert!(s.ok);
        assert!(!s.need_relogin);
        assert!(!s.retryable);
        assert_eq!(s.error, "");

        let f = GlaccResult::fail("登录已过期", true, false);
        assert!(!f.ok);
        assert!(f.need_relogin);
        assert!(!f.retryable);
        assert_eq!(f.error, "登录已过期");

        let t = GlaccResult::fail("网络错误", false, true);
        assert!(t.retryable);
    }

    #[test]
    fn call_result_semantics() {
        let ok = GlaccCallResult::success(42);
        assert!(ok.ok());
        assert_eq!(ok.value, Some(42));
        assert_eq!(ok.reason, GlaccFailReason::None);

        let bad: GlaccCallResult<i32> = GlaccCallResult::fail(GlaccFailReason::BadResponse, "x");
        assert!(!bad.ok());
        assert_eq!(bad.value, None);
        assert_eq!(bad.detail, "x");
    }

    #[test]
    fn http_response_helpers() {
        let plain = GlaccHttpResponse::no_response("连接被拒");
        assert_eq!(plain.reason, GlaccFailReason::NoResponse);
        assert!(plain.response.is_none());

        let fault = GlaccHttpResponse::from_client_fault("缺 dll");
        assert_eq!(fault.reason, GlaccFailReason::ClientError);
    }
}
