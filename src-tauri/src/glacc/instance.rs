// 跨进程领取互斥（零新增依赖，kernel32 命名互斥体）。
//
// 为什么需要：进程内的 SerialGate 只能挡住同一进程里的重复调用。
// 计划任务到点会拉起第二个进程（--scheduled），若此时界面里正在领取，
// 两个进程会同时打同一个出口 IP、同时改同一份凭据文件——违反"多账号串行、不并发"红线。
//
// 用法：领取开始前 ClaimLock::acquire()，拿不到就说明另一个进程正在领取；
// 锁的生命周期 = 句柄存在期，进程崩溃时系统自动回收，不会留下僵尸锁。

use std::ffi::c_void;

#[allow(non_snake_case)]
#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(
        lp_mutex_attributes: *const c_void,
        b_initial_owner: i32,
        lp_name: *const u16,
    ) -> *mut c_void;
    fn CloseHandle(h_object: *mut c_void) -> i32;
    fn GetLastError() -> u32;
}

/// 互斥体名（Local\ 前缀 = 当前登录会话内可见）。
pub const CLAIM_LOCK_NAME: &str = "Local\\glacc-auto-claim-lock";
/// CreateMutexW 在"对象已存在"时置的错误码。
const ERROR_ALREADY_EXISTS: u32 = 183;

/// 领取锁。Drop 时关闭句柄，锁随之释放。
pub struct ClaimLock {
    handle: *mut c_void,
}

// 句柄可跨线程移动（关闭句柄不要求同线程）。
unsafe impl Send for ClaimLock {}
unsafe impl Sync for ClaimLock {}

impl ClaimLock {
    /// 尝试取得领取锁。None = 另一个进程正在领取。
    pub fn acquire() -> Option<Self> {
        let name: Vec<u16> = CLAIM_LOCK_NAME
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: name 以 NUL 结尾且在调用期间存活；不申请初始所有权，
        // 用"对象是否存在"表达占用，避免跨线程释放所有权的麻烦。
        let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if handle.is_null() {
            // 创建失败（极罕见）时按"无法判定"处理：不放行，避免并发。
            return None;
        }
        let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if already {
            unsafe { CloseHandle(handle) };
            return None;
        }
        Some(Self { handle })
    }
}

impl Drop for ClaimLock {
    fn drop(&mut self) {
        // SAFETY: handle 来自 CreateMutexW 且只在这里关闭一次。
        unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同进程内第二次获取必然失败；释放后可再次获取。
    #[test]
    fn second_acquire_fails_until_released() {
        let first = ClaimLock::acquire().expect("首次获取应成功");
        assert!(ClaimLock::acquire().is_none(), "持锁期间不得再次获取");
        drop(first);
        let again = ClaimLock::acquire().expect("释放后应可再次获取");
        drop(again);
    }
}
