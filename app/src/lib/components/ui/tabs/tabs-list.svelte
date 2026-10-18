<script lang="ts">
  import { Tabs as TabsPrimitive } from "bits-ui";
  import type { Snippet } from "svelte";
  import { cn } from "../../../utils/cn";

  let { children, class: className = "" }: { children: Snippet; class?: string } = $props();
</script>

<TabsPrimitive.List class={cn("inline-flex items-center gap-1 rounded-btn bg-muted p-1", className)}>
  {@render children()}
</TabsPrimitive.List>
