<script lang="ts">
  // 弹窗外壳：全窗遮罩 + 居中对齐的对话框，z-index 由调用方指定。
  // 红线：点遮罩绝对不关闭 —— 这里刻意不接 onscrim，遮罩不绑定任何点击行为；
  // 关闭只能由调用方通过右上角 X 或"取消"按钮触发。
  // （bits-ui 的 Modal 已用同一策略，这里保留自有外壳以便逐层控制 z-index。）
  import type { Snippet } from "svelte";
  import { X } from "@lucide/svelte";
  import { cn } from "../utils/cn";

  let {
    open = false,
    zIndex = 50,
    title,
    description,
    closable = true,
    onclose,
    children,
  }: {
    open?: boolean;
    zIndex?: number;
    title?: string;
    description?: string;
    /** false 时连右上角 X 都不显示（用于强制选择的保护类对话框） */
    closable?: boolean;
    onclose?: () => void;
    children: Snippet;
  } = $props();

  const titleId = $props.id();
</script>

{#if open}
  <div class="fixed inset-0 flex items-center justify-center" style:z-index={zIndex}>
    <!-- 遮罩：纯净半透明黑，不做模糊（无边框窗口下 blur 会透出桌面杂色） -->
    <div class="absolute inset-0 bg-black/50" role="presentation"></div>

    <div
      class={cn(
        "relative w-[min(440px,calc(100vw-48px))] rounded-dialog border border-border bg-popover p-6",
        "text-popover-foreground shadow-2xl",
        "animate-in fade-in-0 zoom-in-95 duration-150",
      )}
      role="dialog"
      aria-modal="true"
      aria-labelledby={title ? titleId : undefined}
    >
      {#if closable && onclose}
        <button
          class="absolute right-3 top-3 grid size-7 cursor-default place-items-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground"
          title="关闭"
          aria-label="关闭"
          onclick={onclose}
        >
          <X class="size-4" strokeWidth={1.8} />
        </button>
      {/if}

      {#if title}
        <h2 id={titleId} class="pr-8 text-lg font-semibold text-popover-foreground">{title}</h2>
      {/if}
      {#if description}
        <p class="mt-2 text-sm leading-relaxed text-muted-foreground">{description}</p>
      {/if}

      {@render children()}
    </div>
  </div>
{/if}
