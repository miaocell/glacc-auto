<script lang="ts">
  import { Select as SelectPrimitive } from "bits-ui";
  import { Check, ChevronDown } from "@lucide/svelte";
  import { cn } from "../../../utils/cn";

  export type SelectOption = { value: string; label: string };

  let {
    value = $bindable(""),
    options,
    placeholder = "请选择",
    disabled = false,
    class: className = "",
    "aria-label": ariaLabel,
    onValueChange,
  }: {
    value?: string;
    options: SelectOption[];
    placeholder?: string;
    disabled?: boolean;
    class?: string;
    "aria-label"?: string;
    onValueChange?: (value: string) => void;
  } = $props();
</script>

<SelectPrimitive.Root type="single" bind:value items={options} {disabled} {onValueChange}>
  <SelectPrimitive.Trigger
    aria-label={ariaLabel}
    class={cn(
      "flex h-9 w-full items-center justify-between gap-2 rounded-btn border border-input bg-card",
      "px-3 text-sm text-foreground cursor-pointer select-none",
      "hover:border-primary/45 transition-colors",
      "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/30",
      "data-[disabled]:cursor-not-allowed data-[disabled]:opacity-50",
      className,
    )}
  >
    <SelectPrimitive.Value {placeholder} />
    <ChevronDown class="size-4 shrink-0 text-muted-foreground" />
  </SelectPrimitive.Trigger>

  <SelectPrimitive.Portal>
    <SelectPrimitive.Content
      sideOffset={4}
      class={cn(
        "z-50 min-w-[var(--bits-select-anchor-width)] overflow-hidden rounded-card",
        "border border-border bg-popover text-popover-foreground shadow-lg",
        "data-[state=open]:animate-in data-[state=open]:fade-in-0 data-[state=open]:zoom-in-95",
        "data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=closed]:zoom-out-95",
      )}
    >
      <SelectPrimitive.ScrollUpButton class="flex h-6 items-center justify-center">
        <ChevronDown class="size-3.5 text-muted-foreground rotate-180" />
      </SelectPrimitive.ScrollUpButton>
      <SelectPrimitive.Viewport class="p-1">
        {#each options as option (option.value)}
          <SelectPrimitive.Item
            value={option.value}
            label={option.label}
            class={cn(
              "relative flex h-8 cursor-pointer items-center rounded-md pl-8 pr-2 text-sm outline-none",
              "data-[highlighted]:bg-accent data-[highlighted]:text-accent-foreground",
              "data-[disabled]:cursor-not-allowed data-[disabled]:opacity-50",
            )}
          >
            {#snippet children({ selected })}
              <span class="absolute left-2 flex size-4 items-center justify-center">
                {#if selected}
                  <Check class="size-3.5" />
                {/if}
              </span>
              {option.label}
            {/snippet}
          </SelectPrimitive.Item>
        {/each}
      </SelectPrimitive.Viewport>
      <SelectPrimitive.ScrollDownButton class="flex h-6 items-center justify-center">
        <ChevronDown class="size-3.5 text-muted-foreground" />
      </SelectPrimitive.ScrollDownButton>
    </SelectPrimitive.Content>
  </SelectPrimitive.Portal>
</SelectPrimitive.Root>
