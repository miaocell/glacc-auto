<script lang="ts">
  import { Switch } from "bits-ui";
  import { cn } from "../../../utils/cn";

  let {
    checked = $bindable(false),
    disabled = false,
    class: className = "",
    onCheckedChange,
    "aria-label": ariaLabel,
  }: {
    checked?: boolean;
    disabled?: boolean;
    class?: string;
    onCheckedChange?: (value: boolean) => void;
    "aria-label"?: string;
  } = $props();
</script>

<Switch.Root
  bind:checked
  {disabled}
  {onCheckedChange}
  aria-label={ariaLabel}
  class={cn(
    "inline-flex h-6 w-11 shrink-0 items-center rounded-full border border-transparent",
    "transition-colors duration-150 cursor-pointer",
    "bg-muted-foreground/35 data-[state=checked]:bg-primary",
    "disabled:cursor-not-allowed disabled:opacity-50",
    className,
  )}
>
  <Switch.Thumb
    class={cn(
      "block size-5 rounded-full bg-white shadow-sm transition-transform duration-150",
      "translate-x-0.5 data-[state=checked]:translate-x-[22px]",
    )}
  />
</Switch.Root>
