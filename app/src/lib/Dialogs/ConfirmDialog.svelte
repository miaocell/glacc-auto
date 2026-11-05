<script lang="ts">
  // 通用确认框：标题 + 说明 + 取消 / 确认。
  // 红线：点遮罩不关闭；Esc 视为"取消"（键盘路径，不是遮罩误触）。
  import Modal from "./Modal.svelte";
  import Button from "../components/ui/button/button.svelte";
  import { LoaderCircle } from "@lucide/svelte";

  let {
    open = false,
    zIndex = 60,
    title,
    body,
    confirmText = "确定",
    cancelText = "取消",
    busy = false,
    danger = false,
    onconfirm,
    oncancel,
  }: {
    open?: boolean;
    zIndex?: number;
    title: string;
    body: string;
    confirmText?: string;
    cancelText?: string;
    busy?: boolean;
    /** 主操作为破坏性动作时用错误色 */
    danger?: boolean;
    onconfirm: () => void;
    oncancel: () => void;
  } = $props();

  function onKeydown(e: KeyboardEvent): void {
    if (!open) return;
    if (e.key === "Escape") oncancel();
  }
</script>

<svelte:window onkeydown={onKeydown} />

<Modal {open} {zIndex} {title} description={body} onclose={oncancel}>
  <div class="mt-6 flex justify-end gap-2">
    <Button variant="outline" disabled={busy} onclick={oncancel}>{cancelText}</Button>
    <Button
      variant={danger ? "destructive" : "default"}
      disabled={busy}
      onclick={onconfirm}
    >
      {#if busy}
        <LoaderCircle class="size-4 animate-spin" strokeWidth={2} />
      {/if}
      {confirmText}
    </Button>
  </div>
</Modal>
