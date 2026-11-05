// 前后端共用契约：字段名与 Rust 侧 DTO 的 camelCase 序列化一一对应。

/** 单号本轮状态（对应 Rust SweepState 的字符串值）。 */
export type ClaimState = "pending" | "running" | "done" | "failed" | "skipped";

/** get_accounts 返回的已登录账号摘要。 */
export interface Account {
  key: string;
  /** 脱敏手机号，例如 13******000 */
  label: string;
  isCurrent: boolean;
  /** 最近使用时间（Unix 秒） */
  lastUsed: number;
}

/** get_claim_progress 里的逐号结果。 */
export interface AccountProgress {
  key: string;
  label: string;
  state: ClaimState;
  claimed: number;
  total: number;
  addedMinutes: number;
  balanceMinutes: number;
  message: string | null;
}

/** get_claim_progress 返回的轮次级快照。 */
export interface ClaimProgress {
  running: boolean;
  claimed: number;
  total: number;
  addedMinutes: number;
  balanceMinutes: number;
  statusText: string;
  statusIsError: boolean;
  accounts: AccountProgress[];
  /** 本次运行的滚动事件流（旧到新），首页「运行状态」卡片逐行渲染 */
  events: string[];
}

/** 页面标识：由 NavRail 切换（不引路由库）。 */
export type PageId = "home" | "settings";

/** get_snapshot 返回：登录态 + 可用时长 + 今日任务进度（只读，登录后与点刷新时拉取）。 */
export interface HomeSnapshot {
  /** 本机是否有已登录账号 */
  loggedIn: boolean;
  /** 脱敏手机号；未登录时为空串 */
  label: string;
  balanceMinutes: number;
  /** 余额是否真的取到；false 时界面显示占位符而不是 0 */
  balanceKnown: boolean;
  /** 今日任务已完成次数 */
  claimed: number;
  /** 今日任务总次数 */
  total: number;
  /** 预估可领时长（分钟）：后端按"每阶段总次数 × 阶段奖励分钟"累加的真实值 */
  estimatedMinutes: number;
  statusText: string;
  statusIsError: boolean;
}

/** 主题档位：旧版 ComboBox 的 SelectedIndex 0/1/2。 */
export type ThemeMode = "system" | "light" | "dark";

/** get_settings / save_settings 的载荷（camelCase 对齐 Rust SettingsDto）。 */
export interface AppSettings {
  scheduledEnabled: boolean;
  /** hh:mm，24 小时制 */
  scheduledTime: string;
  serverKey: string;
  intervalMinSec: number;
  intervalMaxSec: number;
  networkRetryCount: number;
  theme: ThemeMode;
  /** 后端版本号，保存时忽略 */
  version: string;
}

/** send_sms_code 返回。 */
export interface SmsCodeResult {
  phoneLabel: string;
  resendSeconds: number;
}

/** login_with_code 返回。 */
export interface LoginResult {
  key: string;
  label: string;
}

/** apply_schedule 返回：计划任务在系统里的真实状态（与 Rust ScheduleStatusDto 对齐）。 */
export interface ScheduleStatus {
  /** 任务处于启用状态（不存在时为 false） */
  enabled: boolean;
  /** 任务真实触发时刻 hh:mm；任务不存在时回落请求值 */
  time: string;
  /** 任务是否存在 */
  exists: boolean;
  /** 本次注册/注销动作是否成功 */
  succeeded: boolean;
  /** 面向用户的说明文案 */
  message: string;
}

/** get_history 返回的单日领取记录（首页"最近 7 天"卡用，前端按本地日期对齐）。 */
export interface HistoryDay {
  /** yyyy-MM-dd（本地日期） */
  date: string;
  /** 当日累计获得的时长（分钟） */
  addedMinutes: number;
  /** 当日是否所有任务完成 */
  completed: boolean;
}

/** 导出 / 导入凭据的结果（与 Rust TransferDto 对齐）。 */
export interface TransferResult {
  /** 用户是否点了取消（true 时其余字段无意义） */
  cancelled: boolean;
  /** 处理是否成功 */
  succeeded: boolean;
  /** 面向用户的说明文案 */
  message: string;
}
