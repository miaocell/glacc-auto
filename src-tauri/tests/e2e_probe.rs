//! 端到端探针：用 glacc::tls 的 FFI 封装，在真实业务形态下访问给梨加速器端点。
//!
//! 本步骤约束的落实方式：
//! - 不改 tls.rs / constants.rs：以 include! 文本复用（源文件零改动，跑前跑后哈希核验）；
//! - 不碰 auth / session / 登录链：只发一个业务读接口，且不含任何账号凭据
//!   （headers 剔除 authorization / userid / user-id / peerid / x-device-id / x-guid）；
//! - 不复制不移动 dll：原位引用 dist 内文件（同步 1b）。
//!
//! 运行：cargo test --test e2e_probe -- --ignored --nocapture

include!("../src/glacc/constants.rs");
include!("../src/glacc/tls.rs");

// ── 设备档案与 UA（直译旧版，作为设备池移植完成前的本地夹具）──

/// 设备档案：GlaccDevicePool 第 3 档（Redmi_2210132C / Android 13 / TKQ1.221114.001 / Chrome 119）。
const DEVICE_MODEL: &str = "2210132C";
const DEVICE_OS_VERSION: i32 = 13;
const DEVICE_OS_BUILD: &str = "TKQ1.221114.001";
const DEVICE_CHROME_VERSION: &str = "119.0.6045.163";

/// 游戏域 UA，逐字直译 GlaccUa.Game。
fn game_ua() -> String {
    format!(
        "xlacc/{} Mozilla/5.0 (Linux; Android {}; {} Build/{}; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/{} Mobile Safari/537.36",
        APP_VERSION, DEVICE_OS_VERSION, DEVICE_MODEL, DEVICE_OS_BUILD, DEVICE_CHROME_VERSION
    )
}

/// 无凭据头部集：取自 GlaccGameClient 的抓包头部，保持原插入序，仅剔除账号相关项。
fn headers_without_credentials() -> Vec<(String, String)> {
    vec![
        ("appid".to_string(), APP_ID.to_string()),
        ("package_name".to_string(), PACKAGE_NAME.to_string()),
        ("package-name".to_string(), PACKAGE_NAME.to_string()),
        ("x-client-type".to_string(), "glacc_android".to_string()),
        ("app-version".to_string(), APP_VERSION.to_string()),
        ("install-channel".to_string(), "official".to_string()),
        ("x-channel-id".to_string(), "official".to_string()),
        ("client_version".to_string(), APP_VERSION.to_string()),
        ("user-agent".to_string(), game_ua()),
    ]
}

/// 12 字段齐全的载荷；指纹随设备档案 OS 版本（与 GlaccDevicePool.IdentifierFor 同规则）。
fn payload_for(url: &str) -> TlsRequestPayload {
    let headers = headers_without_credentials();
    TlsRequestPayload {
        tls_client_identifier: format!("okhttp4_android_{}", DEVICE_OS_VERSION.clamp(7, 13)),
        request_method: "GET".to_string(),
        request_url: url.to_string(),
        request_body: None,
        header_order: Some(headers.iter().map(|(k, _)| k.clone()).collect()),
        headers,
        timeout_seconds: 15,
        follow_redirects: true,
        without_cookie_jar: true,
        is_byte_request: false,
        is_byte_response: false,
        catch_panics: true,
    }
}

/// 取前 n 字节的可打印前缀（不切断 UTF-8 边界）。
fn first_bytes(s: &str, n: usize) -> String {
    let mut end = s.len().min(n);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// 原位定位 dist 内的 dll（与步 1b 同口径；可用 GLACC_TLS_DLL 覆盖）。
fn locate_dist_dll() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("GLACC_TLS_DLL") {
        return std::path::PathBuf::from(p);
    }
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("dist");
    let mut hits: Vec<std::path::PathBuf> = std::fs::read_dir(&dist)
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
            "未找到 {}/**/tls-client.dll（可用 GLACC_TLS_DLL 指定）",
            dist.display()
        )
    })
}

#[test]
#[ignore = "需要网络与 tls-client.dll，手动运行：cargo test --test e2e_probe -- --ignored --nocapture"]
fn probe_task_list_without_credentials() {
    // [0] 加载 dll（原位引用，不复制不移动）
    let dll = locate_dist_dll();
    println!("[0] dll = {}", dll.display());
    initialize_from(&dll).expect("dll 加载失败");

    // [1] 目标：旧版读取任务进度的真实 URL，不带任何凭据
    let url =
        format!("{GAME_BASE}/xlppc.gacs/api/gxsdn/act/advert/mobileGLTaskList?appid={APP_ID}");
    let payload = payload_for(&url);
    println!("[1] GET {url}");
    println!(
        "[1b] 指纹={} 头部序={:?}",
        payload.tls_client_identifier,
        payload.header_order.as_ref().unwrap()
    );

    // [2] 原始响应取证：一次调用即可拿到 id / status / body
    let raw = raw_request(&payload).expect("原生库调用失败");
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => panic!(
            "响应不是 JSON（前 200 字节：{}）：{e}",
            first_bytes(&raw, 200)
        ),
    };
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    let status = v.get("status").and_then(|x| x.as_i64()).unwrap_or(0);
    let body = v
        .get("body")
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string();
    println!("[2] status = {status}");
    println!("[3] body 前 200 字节 = {}", first_bytes(&body, 200));
    println!("[4] 响应 id = {id}");
    if let Ok(j) = serde_json::from_str::<serde_json::Value>(&body) {
        println!(
            "[4b] body.code = {} body.msg = {}",
            j.get("code")
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            j.get("msg")
                .or_else(|| j.get("message"))
                .map(|m| m.to_string())
                .unwrap_or_else(|| "-".into())
        );
    }

    // [5] 按 id 释放原生缓冲（真实 id + 幂等二次释放）
    free(&id);
    free(&id);
    println!("[5] freeMemory(id) 已调用两次（含幂等验证）");

    // [6] send() 全链路封装复验（内部即 raw_request + 解析 + 自动 free）
    let resp = send(&payload).expect("send() 失败");
    println!(
        "[6] send(): status={} body 前 120 字节 = {}",
        resp.status,
        first_bytes(&resp.body, 120)
    );

    assert!(status > 0, "HTTP 层未成功：status={status}");
    assert!(!body.is_empty(), "响应体为空");
    assert!(resp.status > 0, "send() status 异常：{}", resp.status);
}
