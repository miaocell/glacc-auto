<script lang="ts">
  // 轻提示：底部居中的一条，2.6 秒自动消失（计时在 store 里，组件只负责呈现）。
  import { appStore } from "../stores.svelte";
  import { CircleCheck, CircleAlert, Info } from "@lucide/svelte";
  import { cn } from "./utils/cn";

  const kind = $derived(appStore.toast?.kind ?? "info");
  const Icon = $derived(
    kind === "success" ? CircleCheck : kind === "error" ? CircleAlert : Info,
  );
</script>

{#if appStore.toast}
  <div
    class={cn(
      "pointer-events-auto flex items-center gap-2 rounded-card border px-3.5 py-2 text-xs shadow-xl",
      "animate-in fade-in-0 slide-in-from-bottom-2 duration-150",
      appStore.toast.kind === "error"
        ? "border-destructive/40 bg-destructive/10 text-destructive"
        : appStore.toast.kind === "success"
          ? "border-primary/40 bg-primary/10 text-accent-foreground"
          : "border-border bg-popover text-popover-foreground",
    )}
    role="status"
  >
    <Icon class="size-3.5 shrink-0" strokeWidth={1.8} />
    <span class="min-w-0">{appStore.toast.message}</span>
  </div>
{/if}
