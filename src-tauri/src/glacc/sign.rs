// mobileGLTaskPush 请求签名（MD5）。盐作为名为 key 的参数参与字典序排序，extData 不参与签名。

use crate::glacc::constants;

/// sign = md5("key={盐}&masterTaskId={m}&taskId={t}&timestamp={ts}")
/// 拼接按 key 字典序升序：key < masterTaskId < taskId < timestamp，'&' 连接，32 位小写。
pub fn push_sign(master_task_id: i32, task_id: i32, timestamp: i64) -> String {
    let s = format!(
        "key={}&masterTaskId={}&taskId={}&timestamp={}",
        constants::SIGN_KEY,
        master_task_id,
        task_id,
        timestamp
    );
    let digest = md5::compute(s.as_bytes());
    format!("{:x}", digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    // 固定测试向量使用 constants::SIGN_KEY 中的测试专用假钥匙。
    fn self_test() -> bool {
        push_sign(78, 178, 1786116529) == "7539e3580ea553cd906e129145fdd72e"
    }

    #[test]
    fn self_test_sample_matches_legacy() {
        assert_eq!(push_sign(78, 178, 1786116529), "7539e3580ea553cd906e129145fdd72e");
        assert!(self_test());
    }
}
