<script lang="ts">
  import { Tabs as TabsPrimitive } from "bits-ui";
  import type { Snippet } from "svelte";
  import { cn } from "../../../utils/cn";

  let {
    value,
    children,
    class: className = "",
  }: { value: string; children: Snippet; class?: string } = $props();
</script>

<TabsPrimitive.Trigger
  {value}
  class={cn(
    "inline-flex h-7 cursor-pointer items-center gap-1.5 rounded-[6px] px-3 text-xs font-medium",
    "text-muted-foreground transition-colors select-none",
    "hover:text-foreground",
    "data-[state=active]:bg-card data-[state=active]:text-foreground data-[state=active]:shadow-sm",
    className,
  )}
>
  {@render children()}
</TabsPrimitive.Trigger>
