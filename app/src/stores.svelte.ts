// 全局状态：Svelte 5 Runes（$state）。组件里直接读 appStore.xxx，写也统一走方法。
import { api } from "./api";
import { applyTheme as applyThemeClass, nextTheme, THEME_LABEL } from "./lib/theme";
import type {
  Account,
  AppSettings,
  ClaimProgress,
  HistoryDay,
  HomeSnapshot,
  PageId,
  ThemeMode,
  TransferResult,
} from "./types";

const emptyProgress = (): ClaimProgress => ({
  running: false,
  claimed: 0,
  total: 0,
  addedMinutes: 0,
  balanceMinutes: 0,
  statusText: "",
  statusIsError: false,
  accounts: [],
  events: [],
});

const emptySnapshot = (): HomeSnapshot => ({
  loggedIn: false,
  label: "",
  balanceMinutes: 0,
  balanceKnown: false,
  claimed: 0,
  total: 0,
  statusText: "",
  statusIsError: false,
});

const defaultSettings = (): AppSettings => ({
  scheduledEnabled: false,
  scheduledTime: "08:00",
  serverKey: "",
  intervalMinSec: 30,
  intervalMaxSec: 40,
  networkRetryCount: 3,
  theme: "system",
  version: "",
});

/** 重发验证码倒计时（旧版 ResendSeconds = 60）。 */
const RESEND_SECONDS = 60;

class AppStore {
  page = $state<PageId>("home");
  accounts = $state<Account[]>([]);
  progress = $state<ClaimProgress>(emptyProgress());
  /** 首页只读快照（登录态 + 可用时长 + 今日任务）；登录后与点刷新时更新 */
  snapshot = $state<HomeSnapshot>(emptySnapshot());
  snapshotLoading = $state(false);
  snapshotError = $state("");
  /** 最近领取历史（首页"最近 7 天"卡；非关键展示，失败静默为空）。 */
  history = $state<HistoryDay[]>([]);
  /** start_claim 调用中（按钮防连点） */
  starting = $state(false);
  /** 前端侧错误（IPC 不可用、后端拒绝等），与领取结果文案分开 */
  errorText = $state("");

  // ── 轻提示（Toast）──
  /** 就地弹出的轻提示；null = 不显示 */
  toast = $state<{ message: string; kind: "info" | "success" | "error" } | null>(null);
  #toastTimer: ReturnType<typeof setTimeout> | null = null;

  // ── 设置页 ──
  settings = $state<AppSettings>(defaultSettings());
  settingsLoaded = $state(false);
  settingsError = $state("");
  settingsSaving = $state(false);

  // ── 凭据导入 / 导出（入口在首页账号卡，"数据与日志"页已废弃）──
  /** 导入导出进行中（按钮防连点） */
  transferBusy = $state(false);
  /** 导入导出结果提示（成功 / 失败都落这里） */
  transferMessage = $state("");
  transferIsError = $state(false);

  // ── 登录弹窗（手机号 → 验证码两步）──
  loginOpen = $state(false);
  /** false = 第一步手机号；true = 第二步验证码 */
  loginCodeStep = $state(false);
  loginPhone = $state("");
  loginCode = $state("");
  loginBusy = $state(false);
  loginError = $state("");
  /** 已发送验证码的脱敏号码（第二步副标题） */
  loginPhoneLabel = $state("");
  /** 重发倒计时秒数，0 = 可发送 */
  loginResendSeconds = $state(0);
  #loginTimer: ReturnType<typeof setInterval> | null = null;

  // ── 退出登录确认 ──
  logoutConfirmOpen = $state(false);
  logoutBusy = $state(false);

  #pollTimer: ReturnType<typeof setInterval> | null = null;
  /** 设置保存串行队列：连改多个字段时按顺序落盘，后写覆盖前写 */
  #saveQueue: Promise<void> = Promise.resolve();

  /** 首次加载：拉账号列表、当前进度、首页快照与设置。没有 Tauri 环境时只记错误，界面照常渲染。 */
  async init(): Promise<void> {
    try {
      await Promise.all([
        this.refreshAccounts(),
        this.refreshProgress(),
        this.refreshSnapshot(),
        this.loadSettings(),
        this.refreshHistory(),
      ]);
      this.errorText = "";
    } catch (e) {
      this.errorText = messageOf(e);
    }
  }

  async refreshAccounts(): Promise<void> {
    this.accounts = await api.getAccounts();
  }

  /**
   * 拉首页只读快照（可用时长 + 今日任务）。登录后必须调一次，
   * 否则首页只能等到整轮领取结束才有数据可显示。
   */
  async refreshSnapshot(): Promise<void> {
    if (this.snapshotLoading) return;
    this.snapshotLoading = true;
    try {
      this.snapshot = await api.getSnapshot();
      this.snapshotError = "";
    } catch (e) {
      this.snapshotError = messageOf(e);
    } finally {
      this.snapshotLoading = false;
    }
  }

  async refreshProgress(): Promise<void> {
    const next = await api.getClaimProgress();
    const wasRunning = this.progress.running;
    this.progress = next;
    if (wasRunning && !next.running) {
      this.stopPolling();
      await this.refreshAccounts();
      // 本轮跑完余额与任务进度都变了，补拉一次快照与历史
      await this.refreshSnapshot();
      await this.refreshHistory();
    }
  }

  async startClaim(): Promise<void> {
    if (this.starting || this.progress.running) return;
    this.starting = true;
    this.errorText = "";
    try {
      await api.startClaim();
      this.progress = { ...this.progress, running: true, statusText: "正在领取…", statusIsError: false };
      this.startPolling();
    } catch (e) {
      this.errorText = messageOf(e);
      // 状态行已删除：启动失败必须用 Toast 告知，否则错误不可见
      this.showToast(messageOf(e), "error");
    } finally {
      this.starting = false;
    }
  }

  startPolling(): void {
    this.stopPolling();
    // 轮次级快照：领取过程中每秒拉一次，结束后自动停。
    this.#pollTimer = setInterval(() => {
      void this.refreshProgress().catch((e) => {
        this.errorText = messageOf(e);
        this.stopPolling();
      });
    }, 1000);
  }

  stopPolling(): void {
    if (this.#pollTimer !== null) {
      clearInterval(this.#pollTimer);
      this.#pollTimer = null;
    }
  }

  // ── 设置读写 ──

  /** 读设置并应用主题与缩放（启动时调用一次）。 */
  async loadSettings(): Promise<void> {
    try {
      const loaded = await api.getSettings();
      this.settings = loaded;
      this.applyTheme(loaded.theme);
      this.settingsLoaded = true;
      this.settingsError = "";
    } catch (e) {
      this.settingsError = messageOf(e);
      throw e;
    }
  }

  /**
   * 保存设置（原子落盘）。本地值立即生效，落盘按调用顺序串行执行，
   * 避免连改两处时两个 IPC 并发互相覆盖。返回值是后端夹紧后的实际结果。
   */
  saveSettings(patch: Partial<AppSettings>): Promise<void> {
    this.settings = { ...this.settings, ...patch };
    if (patch.theme !== undefined) this.applyTheme(this.settings.theme);
    this.#saveQueue = this.#saveQueue.then(async () => {
      this.settingsSaving = true;
      try {
        // 传纯对象快照：$state 代理不能直接结构化克隆
        const saved = await api.saveSettings({ ...this.settings });
        this.settings = { ...this.settings, ...saved };
        this.applyTheme(this.settings.theme);
        this.settingsError = "";
        // 真正的注册/注销在 save_settings 之后单独走一次 schtasks（最长 15 秒，后端放后台线程）
        if (patch.scheduledEnabled !== undefined || patch.scheduledTime !== undefined) {
          const status = await api.applySchedule(
            this.settings.scheduledEnabled,
            this.settings.scheduledTime,
          );
          // 以后端回读的系统实况为准：失败时开关会自己弹回
          this.settings = {
            ...this.settings,
            scheduledEnabled: status.enabled,
            scheduledTime: status.time,
          };
          if (status.succeeded) {
            // 成功提示只走 Toast，不占设置页布局
            this.showToast(status.message, "success");
            this.settingsError = "";
          } else {
            this.settingsError = status.message;
          }
        }
      } catch (e) {
        this.settingsError = messageOf(e);
      } finally {
        this.settingsSaving = false;
      }
    });
    return this.#saveQueue;
  }

  applyTheme(theme: ThemeMode): void {
    // 主题类挂在 <html> 上（Tailwind dark 变体跟着 class 走），
    // 「跟随系统」也解析成具体明暗再写类，保证同一套令牌生效。
    applyThemeClass(theme);
  }

  /** 主题按钮：跟随系统 → 浅色 → 深色 → 跟随系统，改动即时落盘。 */
  cycleTheme(): void {
    const next = nextTheme(this.settings.theme);
    void this.saveSettings({ theme: next });
  }

  get themeLabel(): string {
    return THEME_LABEL[this.settings.theme];
  }

  // ── 轻提示 ──

  /** 弹一条轻提示，seconds 秒后自动消失；重复调用会重置计时。 */
  showToast(message: string, kind: "info" | "success" | "error" = "info", seconds = 2.6): void {
    if (this.#toastTimer !== null) clearTimeout(this.#toastTimer);
    this.toast = { message, kind };
    this.#toastTimer = setTimeout(() => {
      this.toast = null;
      this.#toastTimer = null;
    }, seconds * 1000);
  }

  dismissToast(): void {
    if (this.#toastTimer !== null) clearTimeout(this.#toastTimer);
    this.#toastTimer = null;
    this.toast = null;
  }

  // ── 首页手动刷新 ──

  /** 首页刷新按钮：账号列表、首页快照、本轮进度、领取历史一起重拉。 */
  async refreshHome(): Promise<void> {
    try {
      await Promise.all([
        this.refreshAccounts(),
        this.refreshSnapshot(),
        this.refreshProgress(),
        this.refreshHistory(),
      ]);
      this.errorText = "";
    } catch (e) {
      this.errorText = messageOf(e);
    }
  }

  // ── 最近领取历史 ──

  /** 拉领取历史。失败静默为空（历史卡是非关键展示，不打扰用户）。 */
  async refreshHistory(): Promise<void> {
    try {
      this.history = await api.getHistory();
    } catch {
      this.history = [];
    }
  }

  // ── 多账号切换 ──

  /**
   * 切换当前激活账号：后端把目标账号标记为"最近使用"（当前账号 = 最近使用的账号），
   * 返回新列表后立刻重拉首页快照与历史。
   * 若切过去的账号登录态已失效（快照报登录类错误），自动弹回登录框。
   */
  async switchTo(key: string): Promise<void> {
    try {
      this.accounts = await api.setActiveAccount(key);
      this.progress = emptyProgress();
      this.snapshot = emptySnapshot();
      await Promise.all([this.refreshSnapshot(), this.refreshHistory()]);
      const text = this.snapshot.statusText ?? "";
      if (this.snapshot.statusIsError && text.includes("登录")) {
        this.showToast("该账号登录态已失效，请重新登录", "error", 3.5);
        this.openLogin();
      } else {
        this.showToast("已切换账号", "success");
      }
    } catch (e) {
      this.errorText = messageOf(e);
      this.showToast(messageOf(e), "error");
    }
  }

  /** 从列表中移除某个账号（删除本机凭据，不可恢复）。 */
  async removeAccount(key: string): Promise<void> {
    try {
      this.accounts = await api.logoutAccount(key);
      this.progress = emptyProgress();
      this.snapshot = emptySnapshot();
      await Promise.all([this.refreshSnapshot(), this.refreshHistory()]);
      this.showToast("已删除该账号", "info");
    } catch (e) {
      this.errorText = messageOf(e);
      this.showToast(messageOf(e), "error");
    }
  }

  // ── 凭据导入 / 导出 ──

  /** 导出：成功后展示后端返回文案（含落盘路径）。取消不算错误。 */
  async exportCredentials(): Promise<void> {
    if (this.transferBusy) return;
    this.transferBusy = true;
    this.transferMessage = "";
    try {
      this.applyTransfer(await api.exportCredentials());
    } catch (e) {
      this.transferIsError = true;
      this.transferMessage = messageOf(e);
    } finally {
      this.transferBusy = false;
    }
  }

  /** 导入：成功后刷新账号列表，让首页立刻反映新账号。 */
  async importCredentials(): Promise<void> {
    if (this.transferBusy) return;
    this.transferBusy = true;
    this.transferMessage = "";
    try {
      const result = await api.importCredentials();
      this.applyTransfer(result);
      if (result.succeeded) await this.refreshAccounts();
    } catch (e) {
      this.transferIsError = true;
      this.transferMessage = messageOf(e);
    } finally {
      this.transferBusy = false;
    }
  }

  applyTransfer(result: TransferResult): void {
    this.transferIsError = !result.cancelled && !result.succeeded;
    this.transferMessage = result.message;
  }

  clearTransferMessage(): void {
    this.transferMessage = "";
    this.transferIsError = false;
  }

  // ── 登录 ──

  openLogin(): void {
    this.stopResendTimer();
    this.loginCodeStep = false;
    this.loginCode = "";
    this.loginError = "";
    this.loginBusy = false;
    this.loginResendSeconds = 0;
    this.loginPhone = "";
    this.loginPhoneLabel = "";
    this.loginOpen = true;
  }

  /** 关闭登录引导：停掉重发倒计时并清错误，重开时回到第一步。 */
  cancelLogin(): void {
    this.loginOpen = false;
    this.stopResendTimer();
    this.loginResendSeconds = 0;
    this.loginError = "";
  }

  /** 发送验证码。成功后进入第二步并开 60 秒重发倒计时。 */
  async sendCode(): Promise<void> {
    if (this.loginBusy || this.loginResendSeconds > 0) return;
    this.loginError = "";
    this.loginBusy = true;
    try {
      const result = await api.sendSmsCode(this.loginPhone);
      this.loginCodeStep = true;
      this.loginCode = "";
      this.loginPhoneLabel = result.phoneLabel;
      this.loginResendSeconds = result.resendSeconds > 0 ? result.resendSeconds : RESEND_SECONDS;
      this.startResendTimer();
    } catch (e) {
      this.loginError = messageOf(e);
    } finally {
      this.loginBusy = false;
    }
  }

  /** 确认登录：成功后刷新账号列表与首页快照并回到首页。 */
  async confirmLogin(): Promise<void> {
    if (this.loginBusy || this.loginCode.trim() === "") return;
    this.loginError = "";
    this.loginBusy = true;
    try {
      await api.loginWithCode(this.loginCode);
      this.loginOpen = false;
      this.stopResendTimer();
      this.loginCode = "";
      this.loginPhone = "";
      this.page = "home";
      await this.refreshAccounts();
      // 登录后立刻同步今日任务与余额：不拉的话首页只能等整轮领取结束才有数据
      await this.refreshSnapshot();
      this.showToast("登录成功，已同步今日数据", "success");
    } catch (e) {
      this.loginError = messageOf(e);
    } finally {
      this.loginBusy = false;
    }
  }

  startResendTimer(): void {
    this.stopResendTimer();
    this.#loginTimer = setInterval(() => {
      this.loginResendSeconds -= 1;
      if (this.loginResendSeconds <= 0) this.stopResendTimer();
    }, 1000);
  }

  stopResendTimer(): void {
    if (this.#loginTimer !== null) {
      clearInterval(this.#loginTimer);
      this.#loginTimer = null;
    }
  }

  /** 重发文案：倒计时中显示剩余秒数。 */
  get resendText(): string {
    return this.loginResendSeconds > 0
      ? `${this.loginResendSeconds} 秒后可重新发送`
      : "重新发送验证码";
  }

  get canSendCode(): boolean {
    return !this.loginBusy && this.loginResendSeconds <= 0;
  }

  get canConfirmLogin(): boolean {
    return !this.loginBusy && this.loginCode.trim() !== "";
  }

  get sentToText(): string {
    return `验证码已发送至 ${this.loginPhoneLabel}，5 分钟内有效`;
  }

  // ── 退出登录 ──

  requestLogout(): void {
    if (this.accounts.length === 0) return;
    this.logoutConfirmOpen = true;
  }

  cancelLogout(): void {
    this.logoutConfirmOpen = false;
  }

  async confirmLogout(): Promise<void> {
    this.logoutBusy = true;
    try {
      const key = this.accounts[0]?.key;
      this.accounts = await api.logoutAccount(key);
      this.logoutConfirmOpen = false;
      this.progress = emptyProgress();
      this.snapshot = emptySnapshot();
      this.showToast("已退出登录", "info");
    } catch (e) {
      this.errorText = messageOf(e);
      this.logoutConfirmOpen = false;
    } finally {
      this.logoutBusy = false;
    }
  }
}

function messageOf(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return String(e);
}

export const appStore = new AppStore();
