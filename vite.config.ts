import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";

// 前端源码在 app/ 下（vite 工程根 = app/），npm 命令统一在项目根执行。
// 构建产物写到项目根的 dist-app/，刻意避开旧的 dist/（那里是 Avalonia 版的打包产物）。
// Svelte 预处理（含 TS）配置在 app/svelte.config.js，插件会自动读取。
export default defineConfig({
  root: "app",
  plugins: [tailwindcss(), svelte()],
  // 让 Tauri CLI 的日志不被 vite 清屏冲掉
  clearScreen: false,
  build: {
    outDir: "../dist-app",
    emptyOutDir: true,
  },
  server: {
    // Tauri 约定端口；被占用时直接失败，不静默换端口（否则 devUrl 会对不上）
    port: 1420,
    strictPort: true,
    watch: {
      // src-tauri 由 cargo 自己监听，避免 vite 触发无谓重载
      ignored: ["**/src-tauri/**"],
    },
  },
});
