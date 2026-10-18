<script lang="ts">
  // 进度条：纯 div 实现，够用且零依赖；数值变化走 CSS 过渡。
  import { cn } from "../../../utils/cn";

  let {
    value = 0,
    class: className = "",
  }: { value?: number; class?: string } = $props();

  const clamped = $derived(Math.max(0, Math.min(100, value)));
</script>

<div class={cn("h-2 w-full overflow-hidden rounded-full bg-muted", className)}>
  <div
    class="h-full rounded-full bg-primary transition-[width] duration-300 ease-out"
    style:width={`${clamped}%`}
  ></div>
</div>
