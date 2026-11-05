<script lang="ts">
  // 左侧导航条：80px 极简，图标按钮各带 Tooltip。
  // 只有 主页 / 设置 两项（"数据与日志"页已废弃，导出/导入入口挪到首页账号卡）。
  // 底部自上而下：GitHub 仓库（open_external）→ 版本号 → 检测更新（点击弹 Toast）。
  import { appStore } from "../stores.svelte";
  import { api } from "../api";
  import Tooltip from "./components/ui/tooltip/tooltip.svelte";
  import { House, Settings, RefreshCw } from "@lucide/svelte";
  import { cn } from "./utils/cn";

  const version = $derived(appStore.settings.version || "0.2.1");

  const REPO_URL = "https://github.com/miaocell/glacc-auto";

  /** 交给后端用系统默认浏览器打开（WebView 里 target=_blank 无效）。 */
  function openRepo(): void {
    void api.openExternal(REPO_URL).catch((e) => appStore.showToast(String(e), "error"));
  }

  const items = [
    { id: "home" as const, label: "主页", icon: House },
    { id: "settings" as const, label: "设置", icon: Settings },
  ];

  function checkUpdate(): void {
    appStore.showToast(`当前已是最新版本 v${version}`);
  }
</script>

<nav
  class="relative flex w-20 shrink-0 flex-col items-center gap-2 border-r border-border/70 bg-rail py-4"
>
  {#each items as item (item.id)}
    <Tooltip content={item.label}>
      {#snippet children({ props })}
        <button
          {...props}
          class={cn(
            "relative grid size-12 cursor-default place-items-center rounded-btn transition-colors",
            appStore.page === item.id
              ? "bg-rail-active text-primary"
              : "text-muted-foreground hover:bg-accent/60 hover:text-foreground",
          )}
          aria-label={item.label}
          onclick={() => {
            appStore.page = item.id;
          }}
        >
          {#if appStore.page === item.id}
            <span
              class="absolute left-0 top-1/2 h-6 w-[3px] -translate-y-1/2 rounded-r-full bg-primary"
            ></span>
          {/if}
          <item.icon class="size-7" strokeWidth={1.8} />
        </button>
      {/snippet}
    </Tooltip>
  {/each}

  <div class="mt-auto flex flex-col items-center gap-2">
    <Tooltip content="GitHub 仓库">
      {#snippet children({ props })}
        <button
          {...props}
          class="grid size-8 cursor-default place-items-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-primary"
          title="GitHub 仓库"
          aria-label="打开 GitHub 仓库"
          onclick={openRepo}
        >
          <!-- lucide 1.x 已移除品牌图标（无 Github 导出），这里内联官方 GitHub 标记 -->
          <svg class="size-[18px]" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true">
            <path
              d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12"
            />
          </svg>
        </button>
      {/snippet}
    </Tooltip>
    <span class="font-mono text-xs leading-none text-muted-foreground/80">v{version}</span>
    <Tooltip content="检测更新">
      {#snippet children({ props })}
        <button
          {...props}
          class="grid size-9 cursor-default place-items-center rounded-md text-muted-foreground transition-colors hover:bg-accent/60 hover:text-primary"
          aria-label="检测更新"
          onclick={checkUpdate}
        >
          <RefreshCw class="size-5" strokeWidth={1.8} />
        </button>
      {/snippet}
    </Tooltip>
  </div>
</nav>
