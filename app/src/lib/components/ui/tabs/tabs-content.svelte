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

<TabsPrimitive.Content value={value} class={cn("min-h-0 outline-none", className)}>
  {@render children()}
</TabsPrimitive.Content>
