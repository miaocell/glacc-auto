// 设备档案：UA 与 TLS 指纹的来源，逐字移植自旧 Avalonia 版
// src\GlaccAuto.Core\Glacc\GlaccDeviceProfile.cs（GlaccDeviceProfile / GlaccDevicePool / GlaccUa）。
// 字段必须是现实中存在的组合（机型+OS+Build+WebView 配套）。

use crate::glacc::constants;

/// 设备档案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceProfile {
    /// 登录域 UA 的 deviceName（如 Redmi_2210132C；小米系与机型同名的机型两值相同）
    pub device_name: &'static str,
    /// 设备型号（deviceModel / WebView UA 中的机型段）
    pub device_model: &'static str,
    /// Android 版本（决定 TLS 预设 okhttp4_android_{n}）
    pub os_version: i32,
    /// 系统 Build 号（WebView UA 的 Build 段）
    pub os_build: &'static str,
    /// 系统 WebView 的 Chrome 版本
    pub chrome_version: &'static str,
    /// 内核版本（登录域 UA 的 Linux 段）
    pub kernel_version: &'static str,
}

/// 内置真实设备档案池（小米/Redmi 系为主，命名与官方 UA 规则一致）。
/// 按手机号哈希确定性选档：同号恒定、异号分散。选档结果不持久化，每次按当前手机号重算。
/// 顺序即索引，必须与旧版 GlaccDevicePool.Profiles 完全一致（选档取模依赖它）。
#[rustfmt::skip]
pub const PROFILES: [DeviceProfile; 12] = [
    DeviceProfile { device_name: "Redmi_25102Rkbec", device_model: "25102RKBEC", os_version: 9, os_build: "PQ3B.190801.07131748", chrome_version: "91.0.4472.114", kernel_version: "4_4_146" },
    DeviceProfile { device_name: "Redmi_2210132C", device_model: "2210132C", os_version: 12, os_build: "SKQ1.211006.001", chrome_version: "107.0.5304.141", kernel_version: "4_19_157" },
    DeviceProfile { device_name: "Redmi_2210132C", device_model: "2210132C", os_version: 13, os_build: "TKQ1.221114.001", chrome_version: "119.0.6045.163", kernel_version: "5_4_210" },
    DeviceProfile { device_name: "2201123C", device_model: "2201123C", os_version: 13, os_build: "TKQ1.220829.002", chrome_version: "119.0.6045.163", kernel_version: "5_10_101" },
    DeviceProfile { device_name: "Redmi_23013RK75C", device_model: "23013RK75C", os_version: 13, os_build: "TKQ1.220905.001", chrome_version: "116.0.5845.163", kernel_version: "5_10_157" },
    DeviceProfile { device_name: "2304DPN8BC", device_model: "2304DPN8BC", os_version: 13, os_build: "TKQ1.221114.001", chrome_version: "119.0.6045.163", kernel_version: "5_10_157" },
    DeviceProfile { device_name: "Redmi_23127PN0CC", device_model: "23127PN0CC", os_version: 14, os_build: "UKQ1.230917.001", chrome_version: "124.0.6367.179", kernel_version: "5_15_104" },
    DeviceProfile { device_name: "24031PN0DC", device_model: "24031PN0DC", os_version: 14, os_build: "UKQ1.230917.001", chrome_version: "124.0.6367.179", kernel_version: "5_15_94" },
    DeviceProfile { device_name: "Redmi_23054RA19C", device_model: "23054RA19C", os_version: 13, os_build: "TKQ1.221114.001", chrome_version: "118.0.5993.65", kernel_version: "5_10_157" },
    DeviceProfile { device_name: "Redmi_22120RN86C", device_model: "22120RN86C", os_version: 13, os_build: "TKQ1.220905.001", chrome_version: "117.0.5938.60", kernel_version: "5_10_101" },
    DeviceProfile { device_name: "2201117TY", device_model: "2201117TY", os_version: 12, os_build: "SKQ1.211006.001", chrome_version: "107.0.5304.141", kernel_version: "4_19_157" },
    DeviceProfile { device_name: "2112123AG", device_model: "2112123AG", os_version: 12, os_build: "SKQ1.211006.001", chrome_version: "103.0.5060.71", kernel_version: "4_19_157" },
];

/// 按手机号（11 位数字串）确定性选档。
/// 算法与旧版逐字一致：SHA256("glacc-device:" + 数字串) 取前 4 字节按【小端】转 u32，再对档案数取模。
/// 字节序必须保持小端（对应旧版 BitConverter.ToUInt32），写成大端会导致选档全错。
pub fn select_for_phone(phone_digits: &str) -> DeviceProfile {
    let hash = sha256(format!("glacc-device:{phone_digits}").as_bytes());
    let idx = u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]) as usize % PROFILES.len();
    PROFILES[idx]
}

/// TLS 预设跟随设备 OS 版本（okhttp4_android_7~13），UA 与 TLS 层自洽。
pub fn identifier_for(device: &DeviceProfile) -> String {
    format!("okhttp4_android_{}", device.os_version.clamp(7, 13))
}

/// 登录域（user.geilijiasu.net，xbase 账号 SDK）UA。拼接格式逐字对照真机抓包。
pub fn ua_auth(d: &DeviceProfile) -> String {
    format!(
        "ANDROID-com.geilijiasu.glacc/{} netWorkType/WIFI appid/22070 \
         deviceName/{} deviceModel/{} OSVersion/{} \
         protocolVersion/301 platformVersion/10 sdkVersion/0 Oauth2Client/0.9 \
         (Linux {}) (JAVA 0)",
        constants::APP_VERSION,
        d.device_name,
        d.device_model,
        d.os_version,
        d.kernel_version
    )
}

/// 游戏域（game-xacc.xunlei.com，RN OkHttp + WebView UA 字符串）UA。
pub fn ua_game(d: &DeviceProfile) -> String {
    format!(
        "xlacc/{} Mozilla/5.0 (Linux; Android {}; {} \
         Build/{}; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 \
         Chrome/{} Mobile Safari/537.36",
        constants::APP_VERSION,
        d.os_version,
        d.device_model,
        d.os_build,
        d.chrome_version
    )
}

// ── 系统加密原语（Windows CNG 直连，零新增 crate）──
// .NET 侧直接用 System.Security.Cryptography（同为系统实现）；Rust 版经 bcrypt.dll 调同一套
// CNG，免去为 SHA256/随机数引入第三方 crate。错误语义同 .NET：系统原语失败抛异常（此处 panic），
// 现实中除非系统损坏不会发生。本节原语供 credentials.rs 复用。

#[link(name = "bcrypt")]
extern "system" {
    fn BCryptOpenAlgorithmProvider(
        ph_algorithm: *mut *mut core::ffi::c_void,
        psz_alg_id: *const u16,
        psz_implementation: *const u16,
        dw_flags: u32,
    ) -> i32;
    fn BCryptCloseAlgorithmProvider(h_algorithm: *mut core::ffi::c_void, dw_flags: u32) -> i32;
    fn BCryptGetProperty(
        h_object: *mut core::ffi::c_void,
        psz_property: *const u16,
        pb_output: *mut u8,
        cb_output: u32,
        pcb_result: *mut u32,
        dw_flags: u32,
    ) -> i32;
    fn BCryptCreateHash(
        h_algorithm: *mut core::ffi::c_void,
        ph_hash: *mut *mut core::ffi::c_void,
        pb_hash_object: *mut u8,
        cb_hash_object: u32,
        pb_secret: *mut u8,
        cb_secret: u32,
        dw_flags: u32,
    ) -> i32;
    fn BCryptHashData(
        h_hash: *mut core::ffi::c_void,
        pb_input: *mut u8,
        cb_input: u32,
        dw_flags: u32,
    ) -> i32;
    fn BCryptFinishHash(
        h_hash: *mut core::ffi::c_void,
        pb_output: *mut u8,
        cb_output: u32,
        dw_flags: u32,
    ) -> i32;
    fn BCryptDestroyHash(h_hash: *mut core::ffi::c_void) -> i32;
    fn BCryptGenRandom(
        h_algorithm: *mut core::ffi::c_void,
        pb_buffer: *mut u8,
        cb_buffer: u32,
        dw_flags: u32,
    ) -> i32;
}

/// BCryptGenRandom 使用系统首选 RNG 时传此标志且句柄为 NULL。
const BCRYPT_USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// NTSTATUS != 0 视为不可恢复的环境故障（与 .NET 抛 CryptographicException 同语义）。
fn cng_check(status: i32, what: &str) {
    if status != 0 {
        panic!("{what} 失败（NTSTATUS=0x{:08X}）", status as u32);
    }
}

/// SHA256（CNG，逐次打开算法提供程序）。等价 .NET SHA256.HashData。
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    debug_assert!(data.len() <= u32::MAX as usize);
    let alg_id = wide("SHA256");
    let prop = wide("ObjectLength");
    let mut alg: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: 标准 CNG 调用序列；句柄在函数内成对创建/释放，缓冲均以长度配套传入。
    unsafe {
        cng_check(
            BCryptOpenAlgorithmProvider(&mut alg, alg_id.as_ptr(), std::ptr::null(), 0),
            "BCryptOpenAlgorithmProvider",
        );
        let mut obj_len: u32 = 0;
        let mut written: u32 = 0;
        cng_check(
            BCryptGetProperty(
                alg,
                prop.as_ptr(),
                &mut obj_len as *mut u32 as *mut u8,
                4,
                &mut written,
                0,
            ),
            "BCryptGetProperty(ObjectLength)",
        );
        let mut obj = vec![0u8; obj_len as usize];
        let mut hash: *mut core::ffi::c_void = std::ptr::null_mut();
        cng_check(
            BCryptCreateHash(
                alg,
                &mut hash,
                obj.as_mut_ptr(),
                obj_len,
                std::ptr::null_mut(),
                0,
                0,
            ),
            "BCryptCreateHash",
        );
        cng_check(
            BCryptHashData(hash, data.as_ptr() as *mut u8, data.len() as u32, 0),
            "BCryptHashData",
        );
        let mut out = [0u8; 32];
        cng_check(
            BCryptFinishHash(hash, out.as_mut_ptr(), 32, 0),
            "BCryptFinishHash",
        );
        let _ = BCryptDestroyHash(hash);
        let _ = BCryptCloseAlgorithmProvider(alg, 0);
        out
    }
}

/// 填充系统随机数。等价 .NET RandomNumberGenerator.Fill。
pub(crate) fn random_bytes(buf: &mut [u8]) {
    // SAFETY: 系统首选 RNG 用法（NULL 句柄 + 固定标志）；缓冲以长度配套传入。
    unsafe {
        cng_check(
            BCryptGenRandom(
                std::ptr::null_mut(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            ),
            "BCryptGenRandom",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA256 正确性：用标准测试向量校验 CNG 直连实现（空串 / "abc" / FIPS 长向量）。
    #[test]
    fn sha256_matches_standard_vectors() {
        let hex = |b: &[u8]| -> String { b.iter().map(|x| format!("{x:02x}")).collect() };
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// 档案池：数量与逐档关键字段须与旧版 GlaccDevicePool.Profiles 一致。
    #[test]
    fn profiles_match_legacy_pool() {
        assert_eq!(PROFILES.len(), 12);
        assert_eq!(PROFILES[0].device_name, "Redmi_25102Rkbec");
        assert_eq!(PROFILES[0].device_model, "25102RKBEC");
        assert_eq!(PROFILES[0].os_version, 9);
        assert_eq!(PROFILES[0].os_build, "PQ3B.190801.07131748");
        assert_eq!(PROFILES[0].chrome_version, "91.0.4472.114");
        assert_eq!(PROFILES[0].kernel_version, "4_4_146");
        assert_eq!(PROFILES[2].kernel_version, "5_4_210");
        assert_eq!(PROFILES[6].os_version, 14);
        assert_eq!(PROFILES[7].os_build, "UKQ1.230917.001");
        assert_eq!(PROFILES[11].chrome_version, "103.0.5060.71");
    }

    /// 选档：用四个已知手机号断言索引与标识（期望值由旧版算法独立复算）。
    /// 覆盖 OS 14（clamp 到 13）与 OS 9 两端。
    #[test]
    fn select_for_phone_matches_legacy() {
        let d = select_for_phone("13800138000");
        assert_eq!(d, PROFILES[1]);
        assert_eq!(d.device_model, "2210132C");
        assert_eq!(d.os_version, 12);
        assert_eq!(identifier_for(&d), "okhttp4_android_12");

        let d = select_for_phone("15212345678");
        assert_eq!(d, PROFILES[8]);
        assert_eq!(d.device_model, "23054RA19C");
        assert_eq!(identifier_for(&d), "okhttp4_android_13");

        // OS 14 档：TLS 标识按 clamp(7,13) 落到 _13
        let d = select_for_phone("13000000000");
        assert_eq!(d, PROFILES[6]);
        assert_eq!(d.device_name, "Redmi_23127PN0CC");
        assert_eq!(d.device_model, "23127PN0CC");
        assert_eq!(d.os_version, 14);
        assert_eq!(identifier_for(&d), "okhttp4_android_13");

        // OS 9 档：落在下界之上，取 _9
        let d = select_for_phone("13000000008");
        assert_eq!(d, PROFILES[0]);
        assert_eq!(identifier_for(&d), "okhttp4_android_9");
    }

    /// IdentifierFor 的 clamp 两端。
    #[test]
    fn identifier_clamps_os_version() {
        let mut d = PROFILES[0];
        d.os_version = 5;
        assert_eq!(identifier_for(&d), "okhttp4_android_7");
        d.os_version = 20;
        assert_eq!(identifier_for(&d), "okhttp4_android_13");
    }

    /// UA 模板：与旧版拼接结果逐字一致（期望串取自旧版 C# 插值的展开）。
    #[test]
    fn ua_templates_match_legacy() {
        let d = select_for_phone("13800138000"); // idx 1：Android 12
        assert_eq!(
            ua_auth(&d),
            "ANDROID-com.geilijiasu.glacc/1.26.8.2 netWorkType/WIFI appid/22070 \
             deviceName/Redmi_2210132C deviceModel/2210132C OSVersion/12 \
             protocolVersion/301 platformVersion/10 sdkVersion/0 Oauth2Client/0.9 \
             (Linux 4_19_157) (JAVA 0)"
        );
        assert_eq!(
            ua_game(&d),
            "xlacc/1.26.8.2 Mozilla/5.0 (Linux; Android 12; 2210132C \
             Build/SKQ1.211006.001; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 \
             Chrome/107.0.5304.141 Mobile Safari/537.36"
        );

        // idx 2（Android 13）：该档 UA 已在步 1b 探针中实发并被服务端接受
        let d = select_for_phone("13000000007");
        assert_eq!(d, PROFILES[2]);
        assert_eq!(
            ua_game(&d),
            "xlacc/1.26.8.2 Mozilla/5.0 (Linux; Android 13; 2210132C \
             Build/TKQ1.221114.001; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 \
             Chrome/119.0.6045.163 Mobile Safari/537.36"
        );
    }
}
