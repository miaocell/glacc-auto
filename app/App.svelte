<script lang="ts">
  // 应用外壳：沉浸式标题栏（32px）+ 左侧导航条（60px）+ 页面区 + 全局弹窗层 + 轻提示。
  // 路由不引库：appStore.page 只区分 主页 / 设置（"数据与日志"页已废弃）。
  import { onMount, tick } from "svelte";
  import TitleBar from "./src/lib/TitleBar.svelte";
  import NavRail from "./src/lib/NavRail.svelte";
  import Toast from "./src/lib/Toast.svelte";
  import Home from "./src/pages/Home.svelte";
  import Settings from "./src/pages/Settings.svelte";
  import LoginDialog from "./src/lib/Dialogs/LoginDialog.svelte";
  import ConfirmDialog from "./src/lib/Dialogs/ConfirmDialog.svelte";
  import Modal from "./src/lib/Dialogs/Modal.svelte";
  import Button from "./src/lib/components/ui/button/button.svelte";
  import { Tooltip } from "bits-ui";
  import { appStore } from "./src/stores.svelte";
  import { windowShow } from "./src/api";

  // 免责声明（开源合规）：首次启动强制弹出，点"我已阅读并同意"后写入 localStorage 永不再弹。
  // 遮罩不可点、无 X、不响应 Esc——唯一出口是同意按钮。
  const DISCLAIMER_KEY = "hasSeenDisclaimer";
  let hasSeenDisclaimer = $state(localStorage.getItem(DISCLAIMER_KEY) === "true");

  function acceptDisclaimer(): void {
    localStorage.setItem(DISCLAIMER_KEY, "true");
    hasSeenDisclaimer = true;
  }

  onMount(() => {
    // 窗口以 visible:false 创建（避免小窗白屏闪烁）：等首帧渲染完再显示。
    // 后端另有 6 秒兜底，前端即使没能调用也不会看不见窗口。
    void tick().then(() => windowShow());
    void appStore.init().catch(() => {
      // init 内部已把错误写进 errorText，这里只防未处理的 rejection
    });
    return () => {
      appStore.stopPolling();
      appStore.stopResendTimer();
    };
  });
</script>

<div class="flex h-screen flex-col overflow-hidden">
  <Tooltip.Provider delayDuration={400}>
    <TitleBar />
    <div class="flex min-h-0 flex-1">
      <NavRail />
      <!-- 页面区不滚动：整窗高度锁死，滚动只在页面内的指定容器里发生 -->
      <main class="relative min-w-0 flex-1 overflow-hidden">
        {#if appStore.page === "home"}
          <Home />
        {:else}
          <Settings />
        {/if}
      </main>
    </div>
  </Tooltip.Provider>
</div>

<!-- 轻提示层：不挡鼠标（pointer-events-none），只有提示条自己可点 -->
<div class="pointer-events-none fixed inset-x-0 bottom-5 z-[80] flex justify-center">
  <Toast />
</div>

<!-- 弹窗层：登录 60 < 退出确认 65 -->
<LoginDialog />
<ConfirmDialog
  open={appStore.logoutConfirmOpen}
  zIndex={65}
  title="退出登录"
  body="退出后将清除本机保存的手机号与登录信息，需重新用短信验证码登录。"
  confirmText="退出登录"
  cancelText="取消"
  danger
  busy={appStore.logoutBusy}
  onconfirm={() => void appStore.confirmLogout()}
  oncancel={() => appStore.cancelLogout()}
/>

<!-- 免责声明弹窗：zIndex 100，高于退出确认（65）与 Toast（80），盖住一切交互 -->
<Modal
  open={!hasSeenDisclaimer}
  zIndex={100}
  title="免责声明"
  closable={false}
  description="本项目仅供个人学习、研究与技术交流，严禁任何商业或盈利性使用。本软件为非官方第三方工具，与任何第三方服务提供方无任何关联。使用可能带来的账号限制、权益损失及法律风险由使用者自行承担。"
>
  <div class="mt-6">
    <Button class="w-full" onclick={acceptDisclaimer}>我已阅读并同意</Button>
  </div>
</Modal>
