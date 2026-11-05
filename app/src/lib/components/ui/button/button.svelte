<script lang="ts">
  import type { Snippet } from "svelte";
  import type { HTMLButtonAttributes } from "svelte/elements";
  import { cn } from "../../../utils/cn";

  type Variant = "default" | "secondary" | "outline" | "ghost" | "destructive";
  type Size = "sm" | "md" | "lg";

  let {
    variant = "default",
    size = "md",
    class: className = "",
    children,
    ...rest
  }: HTMLButtonAttributes & {
    variant?: Variant;
    size?: Size;
    class?: string;
    children?: Snippet;
  } = $props();

  const base =
    "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-btn font-medium " +
    "transition-colors duration-150 cursor-pointer select-none " +
    "disabled:pointer-events-none disabled:opacity-45";

  const variants: Record<Variant, string> = {
    default: "bg-primary text-primary-foreground hover:bg-primary/90",
    secondary: "bg-secondary text-secondary-foreground hover:bg-secondary/75",
    outline: "border border-border bg-transparent hover:bg-accent hover:text-accent-foreground",
    ghost: "hover:bg-accent hover:text-accent-foreground",
    destructive: "bg-destructive text-destructive-foreground hover:bg-destructive/90",
  };

  const sizes: Record<Size, string> = {
    sm: "h-8 px-3 text-xs",
    md: "h-9 px-4 text-sm",
    lg: "h-11 px-6 text-base",
  };
</script>

<button class={cn(base, variants[variant], sizes[size], className)} {...rest}>
  {@render children?.()}
</button>
