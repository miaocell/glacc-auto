<script lang="ts">
  import type { HTMLInputAttributes } from "svelte/elements";
  import { cn } from "../../../utils/cn";

  let { class: className = "", value = $bindable(""), ...rest }: HTMLInputAttributes & {
    class?: string;
  } = $props();
</script>

<input
  class={cn(
    "h-9 w-full min-w-0 rounded-btn border border-input bg-card px-3 text-sm text-foreground",
    "placeholder:text-muted-foreground/70 transition-colors",
    "focus-visible:outline-none focus-visible:border-primary focus-visible:ring-2 focus-visible:ring-ring/30",
    "disabled:cursor-not-allowed disabled:opacity-50",
    className,
  )}
  bind:value
  {...rest}
/>
