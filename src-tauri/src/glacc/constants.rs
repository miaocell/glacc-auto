// 官方客户端常量。
// 用户个人信息（手机号/token/账号 ID）在 GlaccCredentials，持久化于本机 %APPDATA%。

// ── 服务端域名 ──

/// 账号/登录域（xbase OAuth）
pub const AUTH_BASE: &str = "https://user.geilijiasu.net";
/// 业务主域（任务/钱包/推送）
pub const GAME_BASE: &str = "https://game-xacc.xunlei.com";

// ── OAuth 客户端（JS 字符串表明文）──
pub const CLIENT_ID: &str = "abjIHOBRMk89gOKc";
pub const CLIENT_SECRET: &str = "BihWgSGwEfGC_IKQXAAUlQ";
pub const REDIRECT_URI: &str = "xlaccsdk01://xunlei.com/callback?state=harbor";

// ── mobileGLTaskPush 签名盐（Hermes JS module 1591 adStatsSignParams）──
pub const SIGN_KEY: &str = "07f21229eab0bb7d7c4";

// ── 客户端版本指纹 ──
pub const APP_VERSION: &str = "1.26.8.2";
pub const APP_ID: &str = "8";
pub const PACKAGE_NAME: &str = "glacc";
pub const MASTER_TASK_ID: i32 = 52; // 主任务「移动端-点广告得时长」

// UA 不写死：由 GlaccUa 模板 + 设备档案生成，
// 档案按手机号哈希从 GlaccDevicePool 确定性选档，避免全局同一设备指纹。

// ── 服务端业务码（实测所得）──

/// 登录校验失败：JWT 无效/过期/缺失。HTTP 仍为 200，仅 body.code 区分（钱包与 push 已实测）。
pub const AUTH_ERROR_CODE: i32 = 10003;
/// push 请求参数无效（如 taskId 不存在）。鉴权校验先于参数校验。
pub const INVALID_PARAM_CODE: i32 = 10053;

// ── 换算 ──

/// 钱包 score → 可加速时长：80 score = 1 分钟
pub const SCORE_PER_MINUTE: f64 = 80.0;
