<script lang="ts">
  // 设置页（步 7 由原设置浮窗 SettingsModal 改为独立页面）：
  // 内容与浮窗完全一致，外壳换成页面容器，不再有遮罩/Esc/关闭按钮。
  import { appStore } from "../stores.svelte";
  import { api } from "../api";
  import type { ThemeMode } from "../types";
  import Input from "../lib/components/ui/input/input.svelte";
  import Switch from "../lib/components/ui/switch/switch.svelte";
  import Select from "../lib/components/ui/select/select.svelte";
  import { ChevronDown, ExternalLink } from "@lucide/svelte";
  import { cn } from "../lib/utils/cn";

  const settings = $derived(appStore.settings);

  // 定时领取子区展开状态（不持久化，与旧版同语义）
  let scheduleExpanded = $state(false);

  // 领取间隔：本地文本态，失焦时夹紧回写（旧版 CoerceIntervalInput）
  let intervalMinText = $state("30");
  let intervalMaxText = $state("40");

  $effect(() => {
    intervalMinText = String(settings.intervalMinSec);
    intervalMaxText = String(settings.intervalMaxSec);
  });

  const retryOptions = Array.from({ length: 11 }, (_, i) => ({
    value: String(i),
    label: `${i} 次`,
  }));

  const themeOptions: { value: ThemeMode; label: string }[] = [
    { value: "system", label: "跟随系统" },
    { value: "light", label: "浅色" },
    { value: "dark", label: "深色" },
  ];

  /** 间隔输入钳制：数字、1~3600、保持 min ≤ max（越界一侧夹到另一侧）。 */
  function coerceInterval(text: string, isMin: boolean): number {
    const current = isMin ? settings.intervalMinSec : settings.intervalMaxSec;
    const other = isMin ? settings.intervalMaxSec : settings.intervalMinSec;
    let n = Number.parseInt(text, 10);
    if (!Number.isFinite(n)) return current;
    n = Math.min(3600, Math.max(1, n));
    if (isMin && n > other) n = other;
    if (!isMin && n < other) n = other;
    return n;
  }

  function commitInterval(isMin: boolean): void {
    const n = coerceInterval(isMin ? intervalMinText : intervalMaxText, isMin);
    if (isMin) {
      intervalMinText = String(n);
      if (n !== settings.intervalMinSec) void appStore.saveSettings({ intervalMinSec: n });
    } else {
      intervalMaxText = String(n);
      if (n !== settings.intervalMaxSec) void appStore.saveSettings({ intervalMaxSec: n });
    }
  }

  function commitTime(e: Event): void {
    const value = (e.currentTarget as HTMLInputElement).value;
    if (!value || value === settings.scheduledTime) return;
    void appStore.saveSettings({ scheduledTime: value });
  }

  function toggleSchedule(e: MouseEvent): void {
    // 点在开关上不切换展开
    if ((e.target as HTMLElement).closest("[data-switch]")) return;
    scheduleExpanded = !scheduleExpanded;
  }
</script>

<!-- 页面自身锁高（h-full 由父级 main 的 overflow-hidden 保证），
     所有滚动只发生在这个容器内部；pb-16 保证 GitHub 图标不被裁切 -->
<section class="soft-scrollbar mx-auto flex h-full min-h-0 w-full max-w-[680px] flex-col gap-5 px-8 pb-16 pt-6">
  <!-- 领取 -->
  <div class="space-y-3">
    <h3 class="text-xl font-bold text-card-foreground">领取</h3>

    <div class="flex items-center justify-between gap-4 rounded-card border border-border bg-card p-6">
      <div class="min-w-0">
        <div class="text-lg text-card-foreground">领取间隔</div>
        <div class="mt-1 text-sm leading-snug text-muted-foreground">
          每次动作的间隔，在此区间内随机取值
        </div>
      </div>
      <div class="flex shrink-0 items-center gap-2">
        <Input
          class="h-12 w-20 px-2 text-center text-base tabular-nums"
          inputmode="numeric"
          maxlength="4"
          bind:value={intervalMinText}
          onblur={() => commitInterval(true)}
          aria-label="领取间隔下限（秒）"
        />
        <span class="text-base text-muted-foreground">~</span>
        <Input
          class="h-12 w-20 px-2 text-center text-base tabular-nums"
          inputmode="numeric"
          maxlength="4"
          bind:value={intervalMaxText}
          onblur={() => commitInterval(false)}
          aria-label="领取间隔上限（秒）"
        />
        <span class="text-sm text-muted-foreground">秒</span>
      </div>
    </div>

    <div class="flex items-center justify-between gap-4 rounded-card border border-border bg-card p-6">
      <div class="min-w-0">
        <div class="text-lg text-card-foreground">网络重试次数</div>
        <div class="mt-1 text-sm leading-snug text-muted-foreground">
          失败后的自动重试，0 为立即中断
        </div>
      </div>
      <Select
        class="h-12 w-[120px] text-base"
        value={String(settings.networkRetryCount)}
        options={retryOptions}
        aria-label="网络重试次数"
        onValueChange={(v) => void appStore.saveSettings({ networkRetryCount: Number(v) })}
      />
    </div>

    <div class="rounded-card border border-border bg-card">
      <div
        class="flex cursor-default select-none items-center justify-between gap-4 rounded-card p-6 transition-colors hover:bg-accent/40"
        role="button"
        tabindex="0"
        onclick={toggleSchedule}
        onkeydown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            scheduleExpanded = !scheduleExpanded;
          }
        }}
      >
        <div class="min-w-0">
          <div class="text-lg text-card-foreground">定时领取</div>
          <div class="mt-1 text-sm leading-snug text-muted-foreground">
            由 Windows 计划任务在设定时间自动领取
          </div>
        </div>
        <div class="flex shrink-0 items-center gap-3">
          <!-- data-switch 挡住整行 onclick：点开关只切换定时，不折叠面板；箭头不在挡区内，点击可正常展开/收起 -->
          <div data-switch>
            <Switch
              checked={settings.scheduledEnabled}
              aria-label="定时领取"
              onCheckedChange={(v) => void appStore.saveSettings({ scheduledEnabled: v })}
            />
          </div>
          <ChevronDown
            class={cn(
              "size-5 text-muted-foreground transition-transform",
              scheduleExpanded && "rotate-180",
            )}
            strokeWidth={2}
          />
        </div>
      </div>

      {#if scheduleExpanded}
        <div class="space-y-4 border-t border-border/60 px-6 py-5">
          <div class="flex items-center justify-between gap-4">
            <span class="text-base text-card-foreground">每天执行时间</span>
            <Input
              type="time"
              class="h-12 w-[140px] px-3 text-base"
              value={settings.scheduledTime}
              onchange={commitTime}
              aria-label="每天执行时间"
            />
          </div>
          <div class="flex items-center justify-between gap-4">
            <span class="flex items-center gap-1.5 text-base text-card-foreground">
              Server酱
              <a
                class="grid size-7 place-items-center rounded text-primary hover:bg-accent"
                href="https://sct.ftqq.com/"
                target="_blank"
                rel="noreferrer"
                title="打开 Server酱 官网"
                aria-label="打开 Server酱 官网"
                onclick={(e) => {
                  // WebView 里 target=_blank 不会唤起系统浏览器，必须走 open_external IPC
                  e.preventDefault();
                  void api
                    .openExternal("https://sct.ftqq.com/")
                    .catch((err) => appStore.showToast(String(err), "error"));
                }}
              >
                <ExternalLink class="size-4" strokeWidth={2} />
              </a>
            </span>
            <Input
              type="password"
              class="h-12 w-[208px] px-3 text-base"
              placeholder="SendKey"
              value={settings.serverKey}
              onblur={(e) =>
                void appStore.saveSettings({
                  serverKey: (e.currentTarget as HTMLInputElement).value,
                })}
              aria-label="Server酱 SendKey"
            />
          </div>
        </div>
      {/if}
    </div>
  </div>

  <!-- 外观 -->
  <div class="space-y-3">
    <h3 class="text-xl font-bold text-card-foreground">外观</h3>

    <div class="flex items-center justify-between gap-4 rounded-card border border-border bg-card p-6">
      <div class="min-w-0">
        <div class="text-lg text-card-foreground">主题配色</div>
        <div class="mt-1 text-sm text-muted-foreground">跟随系统、浅色或深色</div>
      </div>
      <Select
        class="h-12 w-[140px] text-base"
        value={settings.theme}
        options={themeOptions}
        aria-label="主题配色"
        onValueChange={(v) => void appStore.saveSettings({ theme: v as ThemeMode })}
      />
    </div>
  </div>

  {#if appStore.settingsError}
    <div class="rounded-btn bg-destructive/10 px-4 py-3 text-sm leading-relaxed text-destructive">
      {appStore.settingsError}
    </div>
  {/if}

  <!-- 底部免责声明（定时领取的成功提示已改为 Toast，不再占页面布局） -->
  <div class="mt-auto flex justify-center px-2 pt-4">
    <p class="text-center text-xs font-semibold leading-relaxed text-red-500/80">
      本项目仅供个人学习、研究与技术交流，严禁任何商业或盈利性使用。本软件为非官方第三方工具，与任何第三方服务提供方无任何关联。使用可能带来的账号限制、权益损失及法律风险由使用者自行承担。
    </p>
  </div>
</section>
