<script lang="ts">
  // 登录弹窗：手机号 → 短信验证码两步（与旧版 MainWindow.axaml 的流程一致）。
  // 红线：点遮罩不关闭；两步都只能点"取消"或右上角 X 退出。
  // Esc 仍可退出（键盘快捷路径，不属于遮罩误触）。
  import Modal from "./Modal.svelte";
  import Button from "../components/ui/button/button.svelte";
  import Input from "../components/ui/input/input.svelte";
  import { appStore } from "../../stores.svelte";
  import { ArrowLeft, LoaderCircle } from "@lucide/svelte";

  let phoneInput = $state<HTMLInputElement | null>(null);
  let codeInput = $state<HTMLInputElement | null>(null);

  // 切换步骤 / 打开时聚焦对应输入框
  $effect(() => {
    if (!appStore.loginOpen) return;
    const target = appStore.loginCodeStep ? codeInput : phoneInput;
    target?.focus();
  });

  function onKeydown(e: KeyboardEvent): void {
    if (!appStore.loginOpen) return;
    if (e.key === "Escape") {
      appStore.cancelLogin();
      return;
    }
    if (e.key !== "Enter") return;
    if (appStore.loginCodeStep) {
      if (appStore.canConfirmLogin) void appStore.confirmLogin();
    } else if (!appStore.loginBusy && appStore.loginPhone.trim() !== "") {
      void appStore.sendCode();
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

<Modal
  open={appStore.loginOpen}
  zIndex={60}
  title={appStore.loginCodeStep ? "输入验证码" : "登录账号"}
  description={appStore.loginCodeStep
    ? appStore.sentToText
    : "输入手机号，接收短信验证码"}
  onclose={() => appStore.cancelLogin()}
>
  {#if !appStore.loginCodeStep}
    <Input
      class="mt-5 h-10 text-base tracking-wider"
      type="tel"
      inputmode="numeric"
      maxlength="11"
      placeholder="手机号"
      bind:this={phoneInput}
      bind:value={appStore.loginPhone}
      aria-label="手机号"
    />
  {:else}
    <Input
      class="mt-5 h-10 text-center text-lg tracking-[0.4em]"
      type="text"
      inputmode="numeric"
      maxlength="6"
      placeholder="6 位验证码"
      bind:this={codeInput}
      bind:value={appStore.loginCode}
      aria-label="验证码"
    />
    <div class="mt-2 flex items-center justify-between">
      <button
        class="flex cursor-default items-center gap-1 text-xs text-muted-foreground transition-colors hover:text-primary disabled:cursor-not-allowed disabled:opacity-50"
        disabled={!appStore.canSendCode}
        onclick={() => void appStore.sendCode()}
      >
        <ArrowLeft class="size-3" strokeWidth={2} />
        {appStore.resendText}
      </button>
      <span class="text-[11px] text-muted-foreground/70">5 分钟内有效</span>
    </div>
  {/if}

  {#if appStore.loginError}
    <p class="mt-3 rounded-btn bg-destructive/10 px-3 py-2 text-xs leading-relaxed text-destructive">
      {appStore.loginError}
    </p>
  {/if}

  <div class="mt-6 flex justify-end gap-2">
    <Button variant="outline" onclick={() => appStore.cancelLogin()}>取消</Button>
    {#if !appStore.loginCodeStep}
      <Button
        disabled={appStore.loginBusy || appStore.loginPhone.trim().length === 0}
        onclick={() => void appStore.sendCode()}
      >
        {#if appStore.loginBusy}
          <LoaderCircle class="size-4 animate-spin" strokeWidth={2} />
        {/if}
        发送验证码
      </Button>
    {:else}
      <Button
        disabled={!appStore.canConfirmLogin}
        onclick={() => void appStore.confirmLogin()}
      >
        {#if appStore.loginBusy}
          <LoaderCircle class="size-4 animate-spin" strokeWidth={2} />
        {/if}
        登录
      </Button>
    {/if}
  </div>
</Modal>
