<script lang="ts">
  // 沉浸式标题栏：整条可拖动，左侧应用名，右侧自绘窗口按钮（最小化 + 关闭）。
  // 窗口已固定尺寸且不可最大化（tauri.conf.json: resizable/maximizable 都是 false），
  // 所以这里没有最大化按钮，双击标题栏也不做最大化。
  import { onMount } from "svelte";
  import { effectiveTheme } from "./theme";
  import { appStore } from "../stores.svelte";
  import { windowControl, windowDrag } from "../api";
  import { Minus, Moon, Sun, X } from "@lucide/svelte";

  const systemTheme = window.matchMedia("(prefers-color-scheme: dark)");
  let systemThemeIsDark = $state(systemTheme.matches);
  const actualTheme = $derived(
    appStore.settings.theme === "system"
      ? systemThemeIsDark ? "dark" : "light"
      : appStore.settings.theme,
  );

  onMount(() => {
    const updateSystemTheme = (event: MediaQueryListEvent) => {
      systemThemeIsDark = event.matches;
    };
    systemTheme.addEventListener("change", updateSystemTheme);
    return () => systemTheme.removeEventListener("change", updateSystemTheme);
  });

  function toggleTheme(): void {
    const theme = effectiveTheme() === "dark" ? "light" : "dark";
    void appStore.saveSettings({ theme });
  }

  function onPointerDown(e: MouseEvent) {
    if (e.button !== 0) return;
    // 窗口按钮区不参与拖拽，否则 mousedown 就被拖走、click 不触发
    if ((e.target as HTMLElement).closest("[data-window-control]")) return;
    e.stopPropagation();
    windowDrag();
  }
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<header
  class="flex h-8 shrink-0 select-none items-center bg-background"
  data-tauri-drag-region
  onmousedown={onPointerDown}
>
  <span
    class="ml-4 text-base font-medium tracking-wide text-foreground"
    data-tauri-drag-region
  >
    Glacc Auto
  </span>

  <div class="ml-auto flex h-full" data-window-control>
    <button
      class="flex h-8 w-10 cursor-default items-center justify-center rounded-md text-muted-foreground transition-colors duration-150 hover:bg-black/5 hover:text-foreground"
      title={actualTheme === "light" ? "切换到深色" : "切换到浅色"}
      aria-label={actualTheme === "light" ? "切换到深色" : "切换到浅色"}
      disabled={!appStore.settingsLoaded}
      onclick={toggleTheme}
    >
      {#if actualTheme === "light"}
        <Moon class="size-3.5" strokeWidth={1.6} />
      {:else}
        <Sun class="size-3.5" strokeWidth={1.6} />
      {/if}
    </button>
    <button
      class="flex h-8 w-10 cursor-default items-center justify-center rounded-md text-muted-foreground transition-colors duration-150 hover:bg-black/5 hover:text-foreground"
      title="最小化"
      aria-label="最小化"
      onclick={() => windowControl("minimize")}
    >
      <Minus class="size-3.5" strokeWidth={1.6} />
    </button>
    <button
      class="flex h-8 w-10 cursor-default items-center justify-center rounded-md text-muted-foreground transition-colors duration-150 hover:bg-red-500 hover:text-white"
      title="关闭"
      aria-label="关闭"
      onclick={() => windowControl("close")}
    >
      <X class="size-4" strokeWidth={1.6} />
    </button>
  </div>
</header>
