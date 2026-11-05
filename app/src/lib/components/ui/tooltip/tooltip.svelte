<script lang="ts">
  // 把触发元素交给调用方：bits-ui 的 Trigger 默认渲染 <button>，
  // 而导航项本身就是按钮，必须用 child 片段把触发器属性合并进调用方的元素，
  // 否则会出现 button 套 button。
  // Provider 约束：bits-ui 的 Tooltip 必须在 Tooltip.Provider 上下文内使用，
  // 由 AppRoot 在最外层挂一次（见 App.svelte）。
  import { Tooltip as TooltipPrimitive } from "bits-ui";
  import type { Snippet } from "svelte";
  import { cn } from "../../../utils/cn";

  let {
    content,
    side = "right",
    children,
  }: {
    content: string;
    side?: "top" | "right" | "bottom" | "left";
    children: Snippet<[{ props: Record<string, unknown> }]>;
  } = $props();
</script>

<TooltipPrimitive.Root>
  <TooltipPrimitive.Trigger>
    {#snippet child({ props })}
      {@render children({ props })}
    {/snippet}
  </TooltipPrimitive.Trigger>
  <TooltipPrimitive.Portal>
    <TooltipPrimitive.Content
      {side}
      sideOffset={8}
      class={cn(
        "z-50 rounded-btn bg-foreground px-2.5 py-1.5 text-xs font-medium text-background shadow-md",
        "animate-in fade-in-0 zoom-in-95",
      )}
    >
      {content}
    </TooltipPrimitive.Content>
  </TooltipPrimitive.Portal>
</TooltipPrimitive.Root>
