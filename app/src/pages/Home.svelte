<script lang="ts">
  // 首页（步 12 定稿）：严格两列等宽，卡片高度由内容决定。
  // 左列：账号卡 → 今日任务卡 → 最近 7 天记录卡 → 开始领取按钮（mt-auto 推到最底部，与右列日志底边平齐）。
  // 右列：可用时长卡 → 运行状态日志卡（唯一 flex-1，撑满剩余空间，日志只在内部滚动）。
  // 账号卡带"切换账号"浮窗：列出本机全部账号（上限 3），可置顶激活、删除、添加新账号。
  //
  // 数据取用规则：
  // - appStore.snapshot：get_snapshot 拉到的只读快照（余额 + 今日任务）
  // - appStore.progress：本轮领取的实时进度（含 events 事件流），领取中优先用它
  import { appStore } from "../stores.svelte";
  import Button from "../lib/components/ui/button/button.svelte";
  import Progress from "../lib/components/ui/progress/progress.svelte";
  import Tooltip from "../lib/components/ui/tooltip/tooltip.svelte";
  import {
    ArrowRightLeft,
    Check,
    CircleAlert,
    CircleCheck,
    Hourglass,
    LoaderCircle,
    LogIn,
    LogOut,
    RefreshCw,
    Trash2,
    Upload,
    Download,
    User,
    UserPlus,
  } from "@lucide/svelte";
  import { cn } from "../lib/utils/cn";
  import type { HistoryDay } from "../types";

  const progress = $derived(appStore.progress);
  const snapshot = $derived(appStore.snapshot);
  const accounts = $derived(appStore.accounts);
  const current = $derived(accounts.length > 0 ? accounts[0] : null);
  const loggedIn = $derived(accounts.length > 0 || snapshot.loggedIn);
  const busy = $derived(progress.running || appStore.starting);
  const label = $derived(current?.label ?? snapshot.label ?? "");

  // 可用时长：快照是"当前真实余额"（跑完一轮会重新拉一次），本轮余额只在没快照时兜底
  const balanceKnown = $derived(snapshot.balanceKnown || progress.balanceMinutes > 0);
  const balanceRaw = $derived(
    snapshot.balanceKnown ? snapshot.balanceMinutes : progress.balanceMinutes,
  );
  const balanceHours = $derived(balanceKnown ? Math.floor(Math.round(balanceRaw) / 60) : null);
  const balanceMinutes = $derived(
    balanceKnown ? String(Math.round(balanceRaw) % 60).padStart(2, "0") : null,
  );

  // 今日任务：本轮已有数据时用本轮的（实时），否则用快照
  const claimed = $derived(progress.total > 0 ? progress.claimed : snapshot.claimed);
  const total = $derived(progress.total > 0 ? progress.total : snapshot.total);
  const remaining = $derived(Math.max(0, total - claimed));
  const percent = $derived(total > 0 ? Math.min(100, (claimed / total) * 100) : 0);

  // 预估收益：后端按"每阶段总次数 × 阶段奖励分钟"累加的真实下发值（estimatedMinutes）。
  // 未登录、无任务数据或后端没给出正数时不显示。
  const estimateTotalMinutes = $derived(snapshot.estimatedMinutes);
  const showEstimate = $derived(loggedIn && total > 0 && estimateTotalMinutes > 0);

  const buttonText = $derived.by(() => {
    if (!loggedIn) return "请先登录";
    if (progress.running) return "正在领取";
    if (total > 0 && remaining === 0) return "今日已完成";
    return "开始领取";
  });

  // 运行状态卡片：新事件到达时自动滚到底
  let logBox = $state<HTMLDivElement | null>(null);
  const events = $derived(progress.events);
  $effect(() => {
    const count = events.length;
    if (count > 0 && logBox) logBox.scrollTop = logBox.scrollHeight;
  });

  // 最近 7 天：后端返回"有记录的日子"，前端按本地日期对齐补齐 7 格（无记录 = 虚线圈）。
  const historyDays = $derived.by((): { key: string; label: string; rec?: HistoryDay }[] => {
    const map = new Map(appStore.history.map((h) => [h.date, h]));
    const out: { key: string; label: string; rec?: HistoryDay }[] = [];
    const now = new Date();
    for (let i = 6; i >= 0; i--) {
      const d = new Date(now.getFullYear(), now.getMonth(), now.getDate() - i);
      const key = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(
        d.getDate(),
      ).padStart(2, "0")}`;
      out.push({ key, label: key.slice(5).replace("-", "/"), rec: map.get(key) });
    }
    return out;
  });

  // 未登录点账号卡 → 短信登录弹窗；已登录时右键出菜单（退出登录入口）
  let menu = $state<{ x: number; y: number } | null>(null);
  // 账号切换浮窗
  let switcherOpen = $state(false);

  function onAccountClick(): void {
    if (!loggedIn) appStore.openLogin();
  }

  function onContextMenu(e: MouseEvent): void {
    if (!loggedIn) return;
    e.preventDefault();
    menu = { x: e.clientX, y: e.clientY };
  }

  function closeMenu(e?: MouseEvent): void {
    if (e && (e.target as HTMLElement).closest("[data-account-menu]")) return;
    menu = null;
  }

  /** 切换账号入口：未登录（本机没有账号）时直接呼出登录弹窗，否则开浮窗。 */
  function onSwitchClick(): void {
    if (appStore.accounts.length === 0) {
      appStore.openLogin();
      return;
    }
    switcherOpen = !switcherOpen;
  }

  function closeSwitcher(e?: MouseEvent): void {
    if (e) {
      const t = e.target as HTMLElement;
      if (t.closest("[data-switcher]") || t.closest("[data-switch-trigger]")) return;
    }
    switcherOpen = false;
  }

  async function pickAccount(key: string, isCurrent: boolean): Promise<void> {
    switcherOpen = false;
    if (!isCurrent) await appStore.switchTo(key);
  }

  /** 刷新：强制向后端要一次最新快照（get_snapshot），领取中同样可点。 */
  function refresh(): void {
    void appStore.refreshHome();
  }

  const iconBtn =
    "grid size-8 shrink-0 cursor-default place-items-center rounded-md text-muted-foreground " +
    "transition-colors hover:bg-accent hover:text-foreground disabled:opacity-40 disabled:pointer-events-none";
</script>

<svelte:window
  onclick={(e) => {
    closeMenu(e);
    closeSwitcher(e);
  }}
  onkeydown={(e) => {
    if (e.key === "Escape") {
      closeMenu();
      closeSwitcher();
    }
  }}
/>

<div class="flex h-full gap-6 overflow-hidden p-6">
  <!-- ══ 左侧列 50% ══ -->
  <div class="flex w-1/2 min-w-0 flex-col gap-4">
    <!-- 账号卡：单行布局，左侧大号手机号+状态，右侧 4 个图标 gap-4 -->
    <div class="relative rounded-card border border-border bg-card p-4 shadow-sm">
      <div class="flex items-center justify-between gap-3">
        <button
          class="flex min-w-0 flex-1 cursor-default flex-col gap-0.5 rounded-btn text-left transition-colors hover:bg-accent/50"
          title={loggedIn ? label : "未登录"}
          onclick={onAccountClick}
          oncontextmenu={onContextMenu}
        >
          <span class="truncate text-2xl font-bold leading-tight text-card-foreground">
            {loggedIn ? label : "未登录"}
          </span>
          <span class="truncate text-xs text-muted-foreground">
            {loggedIn ? "已登录 · 右键可退出" : "点击登录账号"}
          </span>
        </button>

        <div class="flex shrink-0 items-center gap-4">
        <Tooltip content="刷新数据">
          {#snippet children({ props })}
            <button
              {...props}
              class={iconBtn}
              aria-label="刷新数据"
              disabled={appStore.snapshotLoading}
              onclick={refresh}
            >
              {#if appStore.snapshotLoading}
                <LoaderCircle class="size-4 animate-spin" strokeWidth={2} />
              {:else}
                <RefreshCw class="size-4" strokeWidth={1.8} />
              {/if}
            </button>
          {/snippet}
        </Tooltip>
        <Tooltip content="导出凭据">
          {#snippet children({ props })}
            <button
              {...props}
              class={iconBtn}
              aria-label="导出凭据"
              disabled={appStore.transferBusy || accounts.length === 0}
              onclick={() => void appStore.exportCredentials()}
            >
              <Download class="size-4" strokeWidth={1.8} />
            </button>
          {/snippet}
        </Tooltip>
        <Tooltip content="导入凭据">
          {#snippet children({ props })}
            <button
              {...props}
              class={iconBtn}
              aria-label="导入凭据"
              disabled={appStore.transferBusy}
              onclick={() => void appStore.importCredentials()}
            >
              <Upload class="size-4" strokeWidth={1.8} />
            </button>
          {/snippet}
        </Tooltip>
        <Tooltip content="切换账号">
          {#snippet children({ props })}
            <button
              {...props}
              class={iconBtn}
              data-switch-trigger
              aria-label="切换账号"
              disabled={busy}
              onclick={onSwitchClick}
            >
              <ArrowRightLeft class="size-4" strokeWidth={1.8} />
            </button>
          {/snippet}
        </Tooltip>
        </div>
      </div>

      {#if appStore.transferMessage}
        <div
          class={cn(
            "flex items-start gap-2 rounded-btn px-2.5 py-1.5 text-xs leading-snug",
            appStore.transferIsError
              ? "bg-destructive/10 text-destructive"
              : "bg-primary/10 text-accent-foreground",
          )}
        >
          {#if appStore.transferIsError}
            <CircleAlert class="mt-px size-3.5 shrink-0" strokeWidth={1.8} />
          {:else}
            <CircleCheck class="mt-px size-3.5 shrink-0" strokeWidth={1.8} />
          {/if}
          <span class="min-w-0 break-words">{appStore.transferMessage}</span>
        </div>
      {/if}

      <!-- 账号切换浮窗：列出本机全部账号（上限 3），可激活 / 删除 / 添加新账号 -->
      {#if switcherOpen}
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div
          class="absolute right-4 top-full z-30 mt-1 w-[264px] rounded-card border border-border bg-popover p-1 shadow-xl"
          onclick={(e) => e.stopPropagation()}
          onkeydown={() => {}}
          role="menu"
          tabindex="-1"
          data-switcher
        >
          <div class="px-2 py-1.5 text-xs text-muted-foreground">
            切换当前账号（本机最多保存 3 个）
          </div>
          {#each accounts as acc (acc.key)}
            <div class="flex items-center gap-1 rounded-md pr-1 transition-colors hover:bg-accent">
              <button
                class="flex min-w-0 flex-1 cursor-default items-center gap-2 rounded-md px-2 py-2 text-left"
                onclick={() => void pickAccount(acc.key, acc.isCurrent)}
              >
                <User class="size-4 shrink-0 text-muted-foreground" strokeWidth={1.8} />
                <span class="min-w-0 flex-1 truncate text-sm text-popover-foreground">
                  {acc.label}
                </span>
                {#if acc.isCurrent}
                  <span class="shrink-0 text-[10px] font-medium text-primary">当前</span>
                {/if}
              </button>
              <button
                class="grid size-7 shrink-0 cursor-default place-items-center rounded-md text-muted-foreground/70 transition-colors hover:bg-destructive/10 hover:text-destructive"
                title="删除该账号（清除本机凭据，不可恢复）"
                aria-label="删除账号 {acc.label}"
                onclick={() => void appStore.removeAccount(acc.key)}
              >
                <Trash2 class="size-3.5" strokeWidth={1.8} />
              </button>
            </div>
          {/each}
          <div class="my-1 h-px bg-border"></div>
          <button
            class="flex w-full cursor-default items-center gap-2 rounded-md px-2 py-2 text-left text-sm text-primary transition-colors hover:bg-accent"
            onclick={() => {
              switcherOpen = false;
              appStore.openLogin();
            }}
          >
            <UserPlus class="size-4" strokeWidth={1.8} />
            添加新账号
          </button>
        </div>
      {/if}

      <!-- 账号右键菜单（界面只展示当前账号，菜单只保留退出登录） -->
      {#if menu}
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div
          class="fixed z-30 min-w-[176px] rounded-card border border-border bg-popover p-1 shadow-xl"
          style:left={`${menu.x}px`}
          style:top={`${menu.y}px`}
          onclick={(e) => e.stopPropagation()}
          onkeydown={() => {}}
          role="menu"
          tabindex="-1"
          data-account-menu
        >
          <button
            class="flex w-full cursor-default items-center gap-2 rounded-md px-2.5 py-2 text-left text-base text-destructive transition-colors hover:bg-destructive/10 disabled:opacity-40"
            disabled={!loggedIn || busy}
            onclick={() => {
              closeMenu();
              appStore.requestLogout();
            }}
          >
            <LogOut class="size-4" strokeWidth={1.8} />
            退出登录
          </button>
        </div>
      {/if}
    </div>

    <!-- 每日任务卡：flex-1 均分剩余高度，内容垂直居中；标题右侧只放"每天预计可领"小字标签，
         第二行右侧固定展示"1440 分钟"（miao 指定写死为纯文本，不读后端动态值） -->
    <div class="flex min-h-0 flex-1 flex-col justify-center rounded-card border border-border bg-card p-4 shadow-sm">
      <div class="flex items-baseline justify-between gap-3">
        <span class="text-xl font-bold text-card-foreground">每日任务</span>
        {#if showEstimate}
          <span class="text-xs text-muted-foreground">每天预计可领</span>
        {/if}
      </div>
      <div class="mt-2 flex items-baseline justify-between gap-3">
        <div class="flex items-baseline gap-1.5 tabular-nums">
          <span class="text-4xl font-bold leading-none text-card-foreground">
            {loggedIn ? claimed : "-"}
          </span>
          <span class="text-lg font-semibold text-muted-foreground/60">/</span>
          <span class="text-2xl font-semibold text-muted-foreground">
            {loggedIn ? total : "-"}
          </span>
        </div>
        {#if showEstimate}
          <span class="shrink-0 text-2xl font-bold tabular-nums text-primary">
            1440 分钟
          </span>
        {/if}
      </div>
      <Progress class="mt-3" value={percent} />
      <div class="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-muted-foreground">
        <span>{percent.toFixed(0)}% 已完成</span>
        {#if progress.addedMinutes > 0}
          <span class="text-primary">本轮已得 {progress.addedMinutes} 分钟</span>
        {:else if loggedIn && remaining > 0}
          <span>还差 {remaining} 次</span>
        {/if}
      </div>
    </div>

    <!-- 最近 7 天记录卡：flex-1 均分剩余高度，标题贴顶，日期行在剩余空间垂直居中（不整卡居中） -->
    <div class="flex min-h-0 flex-1 flex-col rounded-card border border-border bg-card px-4 py-3 shadow-sm">
      <div class="shrink-0 text-lg font-bold text-card-foreground">最近 7 天记录</div>
      <div class="my-auto flex items-start justify-between gap-1 pt-2">
        {#each historyDays as day (day.key)}
          <div
            class="flex min-w-0 flex-1 flex-col items-center gap-1"
            title={day.rec
              ? `${day.label}：+${day.rec.addedMinutes} 分钟${day.rec.completed ? "，已全部完成" : ""}`
              : `${day.label}：无记录`}
          >
            {#if day.rec?.completed}
              <span
                class="grid size-7 place-items-center rounded-full bg-primary text-primary-foreground"
              >
                <Check class="size-4" strokeWidth={3} />
              </span>
            {:else if day.rec}
              <span class="grid size-7 place-items-center rounded-full border border-muted-foreground/40">
                <span class="h-0.5 w-3 rounded bg-muted-foreground/50"></span>
              </span>
            {:else}
              <span class="size-7 rounded-full border border-dashed border-border"></span>
            {/if}
            <span class="text-[10px] leading-none tabular-nums text-muted-foreground">
              {day.label}
            </span>
          </div>
        {/each}
      </div>
    </div>

    <!-- 开始领取：紧贴最近 7 天记录卡下方 -->
    <Button
      size="lg"
      class="h-16 w-full rounded-xl text-xl font-bold shadow-sm"
      disabled={!loggedIn || busy}
      onclick={() => void appStore.startClaim()}
    >
      {#if busy}
        <LoaderCircle class="size-5 animate-spin" strokeWidth={2} />
      {:else if !loggedIn}
        <LogIn class="size-5" strokeWidth={2} />
      {:else if total > 0 && remaining === 0}
        <CircleCheck class="size-5" strokeWidth={2} />
      {/if}
      <span>{buttonText}</span>
    </Button>

    <!-- 定时任务状态：纯展示（不可点击、不跳设置页），无边框透明背景，作为左列收尾 -->
    <div
      class="flex shrink-0 items-center justify-center gap-2.5 px-4 py-1"
      title={appStore.settings.scheduledEnabled
        ? `每天 ${appStore.settings.scheduledTime} 自动领取`
        : "可在设置页开启定时领取"}
    >
      <span class="truncate text-sm text-muted-foreground">
        {appStore.settings.scheduledEnabled ? "定时任务已开启" : "定时任务未开启"}
      </span>
      <span
        class={cn(
          "size-2.5 shrink-0 rounded-full",
          appStore.settings.scheduledEnabled
            ? "bg-green-500 shadow-[0_0_8px_rgba(34,197,94,0.8)]"
            : "bg-red-500 shadow-[0_0_8px_rgba(239,68,68,0.8)]",
        )}
      ></span>
    </div>
  </div>

  <!-- ══ 右侧列 50% ══ -->
  <div class="flex w-1/2 min-w-0 flex-col gap-4">
    <!-- 时长卡：高度自适应 -->
    <div class="flex flex-col items-start justify-center rounded-card border border-border bg-card p-4 shadow-sm">
      <div class="flex items-center gap-2.5">
        <span class="text-base font-medium text-muted-foreground">可用时长</span>
        <span class="flex items-center gap-1 text-sm">
          {#if appStore.snapshotLoading}
            <LoaderCircle class="size-3.5 animate-spin text-muted-foreground" strokeWidth={2} />
            <span class="text-muted-foreground">同步中</span>
          {:else if loggedIn && !balanceKnown}
            <CircleAlert class="size-3.5 text-destructive" strokeWidth={2} />
            <span class="text-destructive">未取到</span>
          {:else if loggedIn}
            <CircleCheck class="size-3.5 text-primary" strokeWidth={2} />
            <span class="text-muted-foreground">已同步</span>
          {:else}
            <span class="text-muted-foreground">登录后显示</span>
          {/if}
        </span>
      </div>
      <div class="mt-1.5 flex items-baseline gap-2">
        <span class="text-6xl font-bold leading-none tabular-nums text-card-foreground">
          {balanceHours ?? "--"}
        </span>
        <span class="text-lg text-muted-foreground">时</span>
        <span class="text-6xl font-bold leading-none tabular-nums text-card-foreground">
          {balanceMinutes ?? "--"}
        </span>
        <span class="text-lg text-muted-foreground">分</span>
      </div>
    </div>

    <!-- 运行日志卡：唯一的 flex-1，撑满右列剩余空间，滚动只发生在内部 -->
    <div class="flex min-h-0 flex-1 flex-col rounded-card border border-border bg-card p-4 shadow-sm">
      <div class="mb-2 flex shrink-0 items-center gap-2">
        <span class="text-lg font-bold text-card-foreground">运行状态</span>
        <span class="flex items-center gap-1.5 text-sm">
          <span
            class={cn(
              "size-2 rounded-full",
              progress.running
                ? "animate-pulse bg-primary"
                : events.length > 0
                  ? "bg-muted-foreground/50"
                  : "bg-muted-foreground/30",
            )}
          ></span>
          <span class={progress.running ? "text-primary" : "text-muted-foreground"}>
            {progress.running ? "领取中" : events.length > 0 ? "已结束" : "空闲"}
          </span>
        </span>
      </div>

      {#if events.length === 0}
        <div class="flex min-h-0 flex-1 flex-col items-center justify-center gap-2 text-center">
          {#if busy}
            <LoaderCircle class="size-6 animate-spin text-primary" strokeWidth={1.8} />
            <span class="text-sm text-muted-foreground">正在准备本轮领取…</span>
          {:else}
            <Hourglass class="size-6 text-muted-foreground/40" strokeWidth={1.6} />
            <span class="text-sm text-muted-foreground">
              点左侧「开始领取」，这里会实时滚动显示进度
            </span>
          {/if}
        </div>
      {:else}
        <!-- 唯一的滚动容器：soft-scrollbar，默认全透明、悬停浮现极淡蓝绿 -->
        <div
          bind:this={logBox}
          class="soft-scrollbar min-h-0 flex-1 font-mono text-sm leading-[1.7] text-muted-foreground"
        >
          {#each events as line, i (i)}
            <div class="rounded px-2 py-0.5 text-card-foreground/90 hover:bg-accent/40">
              {line}
            </div>
          {/each}
        </div>
      {/if}
    </div>
  </div>
</div>
