// 对 Rust 命令的调用封装。
//
// 说明：项目只装了 @tauri-apps/cli，没有 @tauri-apps/api（只为一句 invoke 加一个包
// 不划算）。Tauri 会把 invoke 注入到 window.__TAURI_INTERNALS__，其签名与官方
// @tauri-apps/api/core 的 invoke 一致；将来装包后只需替换本文件内部实现。
import type {
  Account,
  AppSettings,
  ClaimProgress,
  HistoryDay,
  HomeSnapshot,
  LoginResult,
  ScheduleStatus,
  SmsCodeResult,
  TransferResult,
} from "./types";

type InvokeFn = <T>(cmd: string, payload?: Record<string, unknown>) => Promise<T>;

function resolveInvoke(): InvokeFn {
  const internals = (
    window as unknown as {
      __TAURI_INTERNALS__?: { invoke?: InvokeFn };
    }
  ).__TAURI_INTERNALS__;
  if (!internals?.invoke) {
    throw new Error("不在 Tauri 运行环境中，无法调用后端命令");
  }
  return internals.invoke;
}

async function call<T>(cmd: string, payload?: Record<string, unknown>): Promise<T> {
  const invoke = resolveInvoke();
  return invoke<T>(cmd, payload);
}

export const api = {
  getAccounts(): Promise<Account[]> {
    return call<Account[]>("get_accounts");
  },
  /**
   * 首页只读快照：登录态 + 可用时长 + 今日任务进度。
   * 阻塞网络在后端后台线程执行，前端等待期间应显示"同步中"。
   */
  getSnapshot(): Promise<HomeSnapshot> {
    return call<HomeSnapshot>("get_snapshot");
  },
  getClaimProgress(): Promise<ClaimProgress> {
    return call<ClaimProgress>("get_claim_progress");
  },
  startClaim(): Promise<string> {
    return call<string>("start_claim");
  },
  getSettings(): Promise<AppSettings> {
    return call<AppSettings>("get_settings");
  },
  saveSettings(settings: AppSettings): Promise<AppSettings> {
    return call<AppSettings>("save_settings", { input: settings });
  },
  /**
   * 应用定时领取设置：开启时向 Windows 注册 glacc-auto-claim 计划任务，关闭时删除。
   * 返回值是系统的真实状态（注册失败时 enabled 会回到 false）。
   */
  applySchedule(enabled: boolean, time: string): Promise<ScheduleStatus> {
    return call<ScheduleStatus>("apply_schedule", { enabled, time });
  },
  sendSmsCode(phone: string): Promise<SmsCodeResult> {
    return call<SmsCodeResult>("send_sms_code", { phone });
  },
  loginWithCode(code: string): Promise<LoginResult> {
    return call<LoginResult>("login_with_code", { code });
  },
  logoutAccount(key?: string): Promise<Account[]> {
    return call<Account[]>("logout_account", key ? { key } : {});
  },
  /** 切换当前激活账号：把目标账号标记为"最近使用"。返回切换后的账号列表。 */
  setActiveAccount(key: string): Promise<Account[]> {
    return call<Account[]>("set_active_account", { key });
  },
  /**
   * 用系统默认浏览器打开 https 链接（设置页底部 GitHub 图标用）。
   * 后端只放行 https，其它协议直接报错。
   */
  openExternal(url: string): Promise<void> {
    return call<void>("open_external", { url });
  },
  /** 最近领取历史（近 30 天内实际有记录的日子，按日期升序；前端只取本地近 7 天渲染）。 */
  getHistory(): Promise<HistoryDay[]> {
    return call<HistoryDay[]>("get_history");
  },
  /** 导出凭据到用户选择的文件；取消时 succeeded 为 false 且 cancelled 为 true。 */
  exportCredentials(): Promise<TransferResult> {
    return call<TransferResult>("export_credentials");
  },
  /** 从用户选择的文件导入凭据。 */
  importCredentials(): Promise<TransferResult> {
    return call<TransferResult>("import_credentials");
  },
};

/** 自绘标题栏的窗口控制。 */
export type WindowAction = "minimize" | "toggleMaximize" | "close";

const WINDOW_COMMANDS: Record<WindowAction, string> = {
  minimize: "window_minimize",
  toggleMaximize: "window_toggle_maximize",
  close: "window_close",
};

export function windowControl(action: WindowAction): void {
  void call<void>(WINDOW_COMMANDS[action]).catch(() => {
    // 窗口控制失败不打扰用户：界面本身不受影响。
  });
}

export function windowDrag(): void {
  void call<void>("window_drag").catch(() => {});
}

/**
 * 显示窗口：窗口以 visible:false 创建（避免小窗白屏闪烁），
 * 前端首帧渲染完成后调它显示。失败不打扰用户——后端 6 秒兜底会强制显示。
 */
export function windowShow(): void {
  void call<void>("window_show").catch(() => {});
}
