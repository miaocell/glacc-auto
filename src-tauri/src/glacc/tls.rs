// tls-client 原生库（bogdanfinn/tls-client cffi）最小 FFI 封装，与旧 Avalonia 版
// src\GlaccAuto.Core\Glacc\GlaccTls.cs 一一对应。
// 全部业务 HTTP 经此栈发出（OkHttp/Android 指纹），避免暴露系统 TLS 栈指纹。
// 导出为无会话模式：每个请求载荷自带完整配置，返回响应 JSON（含 id），用完 freeMemory(id)。
//
// 加载方式与旧版逐项一致：LoadLibraryW 定位 → GetProcAddress 取 "request" / "freeMemory"
// （C# 同用这两个 kernel32 函数；Rust 侧因此不需要 libloading 等第三方 crate）。
// 调用约定：旧版声明 Cdecl；x64 上只有一种调用约定，Rust 的 extern "C" 与其等价。
// 模块句柄常驻进程、永不 FreeLibrary（与旧版的静态委托同寿命）。

use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Serialize;

/// tls-client cffi 请求载荷。
/// 字段名与旧版 TlsRequestPayload 逐字一致（对应 Go 侧 RequestInput JSON tag）。
#[derive(Debug, Clone, Serialize)]
pub struct TlsRequestPayload {
    #[serde(rename = "tlsClientIdentifier")]
    pub tls_client_identifier: String,
    #[serde(rename = "requestMethod")]
    pub request_method: String,
    #[serde(rename = "requestUrl")]
    pub request_url: String,
    #[serde(rename = "requestBody")]
    pub request_body: Option<String>,
    /// 保序键值对，对应旧版 Dictionary 的插入序语义。
    /// 顺序有意义：headerOrder 由它派生（旧版即 [..headers.Keys]），会直接决定发出的头部顺序与 TLS 指纹。
    #[serde(rename = "headers", serialize_with = "serialize_headers")]
    pub headers: Vec<(String, String)>,
    #[serde(rename = "headerOrder")]
    pub header_order: Option<Vec<String>>,
    #[serde(rename = "timeoutSeconds")]
    pub timeout_seconds: i32,
    #[serde(rename = "followRedirects")]
    pub follow_redirects: bool,
    #[serde(rename = "withoutCookieJar")]
    pub without_cookie_jar: bool,
    #[serde(rename = "isByteRequest")]
    pub is_byte_request: bool,
    #[serde(rename = "isByteResponse")]
    pub is_byte_response: bool,
    #[serde(rename = "catchPanics")]
    pub catch_panics: bool,
}

/// 按旧版 Dictionary 序列化语义把保序键值对写成 JSON 对象（非数组）。
fn serialize_headers<S>(headers: &[(String, String)], s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;
    let mut map = s.serialize_map(Some(headers.len()))?;
    for (k, v) in headers {
        map.serialize_entry(k, v)?;
    }
    map.end()
}

/// 原生库响应（仅业务需要的字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsResponse {
    pub status: i32,
    pub body: String,
}

/// 发送失败的两类故障，对应旧版"抛 GlaccTlsUnavailableException"与"返回 null、原因经 out error 带出"。
#[derive(Debug)]
pub enum TlsError {
    /// 本机指纹库故障（缺失/无法加载/缺导出函数）：属客户端环境故障，与网络故障区分。
    Unavailable(String),
    /// 请求失败（网络不可达/超时/响应非 JSON 等）。
    Request(String),
}

impl std::fmt::Display for TlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsError::Unavailable(msg) | TlsError::Request(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for TlsError {}

// ── 原生库入口 ──

type RequestFn = unsafe extern "C" fn(*const c_char) -> *mut c_char;
type FreeMemoryFn = unsafe extern "C" fn(*const c_char);

#[allow(non_snake_case)]
#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(lp_file_name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, proc_name: *const u8) -> *mut c_void;
    fn GetLastError() -> u32;
}

/// 已加载的原生库入口。只存函数指针：模块句柄自加载起常驻进程，不做 FreeLibrary。
struct TlsModule {
    request: RequestFn,
    free_memory: FreeMemoryFn,
}

static MODULE: OnceLock<TlsModule> = OnceLock::new();

/// 定位并加载原生库；失败返回明确错误（绝不静默降级回系统 TLS 栈）。
/// 探测顺序：exe 同目录 → exe 目录下的 resources\ → runtimes\tls-client\win\x64\；
/// 开发构建再补一条"仓库 dist 目录"的兜底（打包构建绝不走这条路）。
pub fn initialize() -> Result<(), TlsError> {
    if MODULE.get().is_some() {
        return Ok(());
    }
    let mut probes: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            probes.push(dir.join("tls-client.dll"));
            // Tauri 打包的资源目录可能落在 exe 同级的 resources\ 下（视打包器而定）
            probes.push(dir.join("resources").join("tls-client.dll"));
            probes.push(
                dir.join("runtimes")
                    .join("tls-client")
                    .join("win")
                    .join("x64")
                    .join("tls-client.dll"),
            );
        }
    }
    if cfg!(debug_assertions) {
        probes.extend(dev_dll_candidates());
    }
    let found = probes.iter().find(|p| p.is_file()).ok_or_else(|| {
        TlsError::Unavailable(
            "未找到 TLS 指纹库 tls-client.dll，请重新安装应用（勿删除安装目录下的 dll）。"
                .to_string(),
        )
    })?;
    initialize_from(found)
}

/// 开发期兜底：仓库 `dist\*\tls-client.dll`（打包后的 exe 永远走 exe 同目录）。
/// 开发时的 exe 在 `src-tauri\target\debug` 下，正式打包不含本分支。
fn dev_dll_candidates() -> Vec<PathBuf> {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("dist");
    let mut hits: Vec<PathBuf> = std::fs::read_dir(&dist)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path().join("tls-client.dll"))
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default();
    hits.sort();
    hits
}

/// 按显式路径加载（测试/开发用；生产走 initialize 的探测顺序）。与旧版一致：首次生效。
pub fn initialize_from(path: &Path) -> Result<(), TlsError> {
    if MODULE.get().is_some() {
        return Ok(());
    }
    let module = load(path)?;
    let _ = MODULE.set(module);
    Ok(())
}

fn load(path: &Path) -> Result<TlsModule, TlsError> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: wide 以 NUL 结尾且在调用期间存活；加载成功后模块常驻，绝不 FreeLibrary（与旧版同）。
    let module = unsafe { LoadLibraryW(wide.as_ptr()) };
    if module.is_null() {
        let err = unsafe { GetLastError() };
        return Err(TlsError::Unavailable(format!(
            "加载 TLS 指纹库失败：{}（Win32Error={}）",
            path.display(),
            err
        )));
    }
    // SAFETY: module 为刚加载成功的有效句柄；两个导出名均以 NUL 结尾。
    let request: RequestFn = unsafe { std::mem::transmute(symbol(module, b"request\0")?) };
    let free_memory: FreeMemoryFn =
        unsafe { std::mem::transmute(symbol(module, b"freeMemory\0")?) };
    Ok(TlsModule {
        request,
        free_memory,
    })
}

/// 取导出函数指针；缺失即报错（绝不静默降级）。
unsafe fn symbol(module: *mut c_void, name: &[u8]) -> Result<*mut c_void, TlsError> {
    let ptr = GetProcAddress(module, name.as_ptr());
    if ptr.is_null() {
        return Err(TlsError::Unavailable(format!(
            "TLS 指纹库缺少导出函数：{}",
            String::from_utf8_lossy(&name[..name.len() - 1])
        )));
    }
    Ok(ptr)
}

// ── 调用 ──

/// 执行一次 HTTP 请求（阻塞调用，调用方须放在后台线程）。
/// 失败返回明确原因，见 TlsError。
pub fn send(payload: &TlsRequestPayload) -> Result<TlsResponse, TlsError> {
    let raw = raw_request(payload)?;
    // 响应 JSON 来自网络对端（也可能被网关/代理替换成 HTML 或异常形状），解析随时可能失败：
    // id 必须**最先**取出，否则解析在 id 取出前失败时 finally 拿不到 id，
    // 原生缓冲永不释放 —— 常驻进程会无上限泄漏，且日志里毫无线索。
    let value: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return Err(TlsError::Request(format!("响应 JSON 解析失败：{e}"))),
    };
    // 非 JSON 对象：与旧版一致，此时取不到 id，不释放。
    if !value.is_object() {
        return Err(TlsError::Request("原生库响应不是 JSON 对象".to_string()));
    }
    let id = value.get("id").and_then(|v| v.as_str()).map(str::to_owned);
    let result = interpret(&value);
    // finally 语义：只要取到了 id 就释放（无论成败）。
    if let Some(id) = id.as_deref().filter(|s| !s.is_empty()) {
        free(id);
    }
    result
}

/// FFI 调用层：序列化载荷 → 补 NUL → request → 读回响应 JSON 字符串。
/// 响应缓冲归原生库所有，只能经 freeMemory(id) 释放，绝不可自行 free。
fn raw_request(payload: &TlsRequestPayload) -> Result<String, TlsError> {
    initialize()?;
    let module = MODULE.get().expect("initialize() 成功后 MODULE 必已设置");
    // 与旧版一致：WriteIndented 风格序列化（Go 侧按 JSON 解析，缩进不影响语义）。
    let json = serde_json::to_string_pretty(payload)
        .map_err(|e| TlsError::Request(format!("载荷序列化失败：{e}")))?;
    // Go 侧按 C 字符串读取，补 NUL 终止符；JSON 转义保证不会出现内嵌 NUL。
    let c_json = CString::new(json).map_err(|e| TlsError::Request(format!("载荷含 NUL：{e}")))?;
    // SAFETY: 入参为有效 NUL 结尾的 C 字符串且在调用期间存活；返回指针为原生库注册的缓冲，
    // 这里只读借用（CStr），释放走 freeMemory(id)。
    let ptr = unsafe { (module.request)(c_json.as_ptr()) };
    if ptr.is_null() {
        return Err(TlsError::Request("TLS 库返回空指针".to_string()));
    }
    // 与旧版 Marshal.PtrToStringUTF8 同语义：非法序列替换为 U+FFFD。
    let raw = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    if raw.is_empty() {
        return Err(TlsError::Request("TLS 库返回空响应".to_string()));
    }
    Ok(raw)
}

/// 按响应对象解读 status/body（对应旧版 Send 中解析段的逐条判断）。
fn interpret(value: &serde_json::Value) -> Result<TlsResponse, TlsError> {
    // status 非整数（如网关返回字符串/浮点）时按"未取得状态"处理，绝不因取整失败而抛。
    let status = value
        .get("status")
        .and_then(|v| v.as_i64())
        .filter(|n| *n >= i32::MIN as i64 && *n <= i32::MAX as i64)
        .map(|n| n as i32)
        .unwrap_or(0);
    let body = value
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // 传输层未成功（DNS 不解析/连接被拒/超时等）不发生 HTTP 交互：原生库以 status=0 表达，
    // Go 侧错误文本放在 body。必须与"取到响应但解析失败"区分，否则会把断网误报成"接口已变更"。
    if status <= 0 {
        return Err(TlsError::Request(if body.is_empty() {
            "网络不可达或超时（原生库未给出原因）".to_string()
        } else {
            body
        }));
    }
    Ok(TlsResponse { status, body })
}

/// 释放原生缓冲（按 id）。未知 id 时原生库静默返回，幂等安全。
fn free(id: &str) {
    let Some(module) = MODULE.get() else { return };
    // SAFETY: id 为原生库自身生成的 UUID（无内嵌 NUL 时构造必成功）；对任意 id 调用均安全。
    if let Ok(c_id) = CString::new(id) {
        unsafe { (module.free_memory)(c_id.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(s: &str, n: usize) -> String {
        s.chars().take(n).collect()
    }

    /// 载荷形态保真：字段名与结构须与旧版 TlsRequestPayload 序列化结果一致。
    /// headers 必须是 JSON 对象（旧版 Dictionary），headerOrder 必须是数组且与 headers 插入序一致。
    #[test]
    fn payload_json_shape_matches_legacy() {
        let headers: Vec<(String, String)> = vec![
            ("appid".to_string(), "8".to_string()),
            ("user-agent".to_string(), "UA".to_string()),
            ("authorization".to_string(), "Bearer x".to_string()),
        ];
        let payload = TlsRequestPayload {
            tls_client_identifier: "okhttp4_android_13".to_string(),
            request_method: "POST".to_string(),
            request_url: "https://example.com/a".to_string(),
            request_body: Some("{}".to_string()),
            header_order: Some(headers.iter().map(|(k, _)| k.clone()).collect()),
            headers,
            timeout_seconds: 15,
            follow_redirects: true,
            without_cookie_jar: true,
            is_byte_request: false,
            is_byte_response: false,
            catch_panics: true,
        };
        let v: serde_json::Value = serde_json::to_value(&payload).unwrap();
        let obj = v.as_object().unwrap();

        // 1) 12 个字段名与旧版逐字一致
        for key in [
            "tlsClientIdentifier",
            "requestMethod",
            "requestUrl",
            "requestBody",
            "headers",
            "headerOrder",
            "timeoutSeconds",
            "followRedirects",
            "withoutCookieJar",
            "isByteRequest",
            "isByteResponse",
            "catchPanics",
        ] {
            assert!(obj.contains_key(key), "缺字段：{key}");
        }
        assert_eq!(obj.len(), 12, "字段数应为 12");

        // 2) headers 是对象（非数组），键值原样
        let h = obj["headers"].as_object().expect("headers 应为 JSON 对象");
        assert_eq!(h.len(), 3);
        assert_eq!(h["appid"], "8");
        assert_eq!(h["authorization"], "Bearer x");

        // 3) headerOrder 与插入序一致（旧版 [.. headers.Keys]）
        let order: Vec<&str> = obj["headerOrder"]
            .as_array()
            .expect("headerOrder 应为数组")
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(order, vec!["appid", "user-agent", "authorization"]);

        // 4) 无会话模式与旧版调用点一致：withoutCookieJar=true、不用 isByte*、catchPanics=true
        assert_eq!(obj["withoutCookieJar"], true);
        assert_eq!(obj["isByteRequest"], false);
        assert_eq!(obj["isByteResponse"], false);
        assert_eq!(obj["catchPanics"], true);
        assert_eq!(obj["timeoutSeconds"], 15);
        assert_eq!(obj["tlsClientIdentifier"], "okhttp4_android_13");
    }

    /// 最小验证：原位加载 dist 内的 dll（不复制、不移动），发一个不依赖登录态的最简请求，
    /// 确认能加载、能调用、能返回、能释放。默认忽略（需要网络），显式运行：
    ///   cargo test --lib glacc::tls -- --ignored --nocapture
    #[test]
    #[ignore = "需要网络与 tls-client.dll，手动运行"]
    fn minimal_request_roundtrip() {
        let dll = locate_dev_dll();
        println!("使用 dll：{}", dll.display());
        initialize_from(&dll).expect("dll 加载失败");

        let headers: Vec<(String, String)> = vec![(
            "user-agent".to_string(),
            "Mozilla/5.0 (Linux; Android 13; 2210132C Build/TKQ1.221114.001; wv) \
             AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/119.0.6045.163 \
             Mobile Safari/537.36"
                .to_string(),
        )];
        let payload = TlsRequestPayload {
            tls_client_identifier: "okhttp4_android_13".to_string(),
            request_method: "GET".to_string(),
            request_url: "https://www.baidu.com/".to_string(),
            request_body: None,
            header_order: Some(headers.iter().map(|(k, _)| k.clone()).collect()),
            headers,
            timeout_seconds: 15,
            follow_redirects: true,
            without_cookie_jar: true,
            is_byte_request: false,
            is_byte_response: false,
            catch_panics: true,
        };

        // 1) 直接观察原生库原始响应：id / status / body 三项都要有
        let raw = raw_request(&payload).expect("原生库调用失败");
        println!("原始响应前 300 字符：{}", preview(&raw, 300));
        let v: serde_json::Value = serde_json::from_str(&raw).expect("响应不是 JSON");
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .expect("响应缺 id")
            .to_string();
        assert!(!id.is_empty(), "id 不应为空");
        assert!(
            v.get("status").and_then(|x| x.as_i64()).unwrap_or(0) > 0,
            "status 应为正数"
        );
        assert!(
            !v.get("body")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .is_empty(),
            "body 不应为空"
        );
        // 2) 显式走 freeMemory：真实 id 释放 + 二次释放（幂等）
        free(&id);
        free(&id);

        // 3) 全链路 send()（内含自动释放）再跑一次
        let resp = send(&payload).expect("send() 失败");
        println!(
            "send(): status={} body 前 120 字符：{}",
            resp.status,
            preview(&resp.body, 120)
        );
        assert!(resp.status > 0, "send() status 应为正数");
        assert!(!resp.body.is_empty(), "send() body 不应为空");
    }

    /// 定位开发期 dll：优先 GLACC_TLS_DLL 环境变量，否则扫 dist\*\tls-client.dll。
    fn locate_dev_dll() -> PathBuf {
        if let Ok(p) = std::env::var("GLACC_TLS_DLL") {
            return PathBuf::from(p);
        }
        let dist = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("dist");
        let mut hits: Vec<PathBuf> = std::fs::read_dir(&dist)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path().join("tls-client.dll"))
                    .filter(|p| p.is_file())
                    .collect()
            })
            .unwrap_or_default();
        hits.sort();
        hits.into_iter().next().unwrap_or_else(|| {
            panic!(
                "未找到 {}/*/tls-client.dll（可用 GLACC_TLS_DLL 指定）",
                dist.display()
            )
        })
    }
}
