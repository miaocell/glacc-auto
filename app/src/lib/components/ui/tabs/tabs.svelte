<script lang="ts">
  import { Tabs as TabsPrimitive } from "bits-ui";
  import type { Snippet } from "svelte";
  import { cn } from "../../../utils/cn";

  let {
    value = $bindable(""),
    children,
    class: className = "",
    onValueChange,
  }: {
    value?: string;
    children: Snippet;
    class?: string;
    onValueChange?: (value: string) => void;
  } = $props();
</script>

<TabsPrimitive.Root bind:value {onValueChange} class={cn("flex min-h-0 flex-col", className)}>
  {@render children()}
</TabsPrimitive.Root>
