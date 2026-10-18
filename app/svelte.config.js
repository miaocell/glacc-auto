import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

// Svelte 5 + TypeScript：预处理链由 vitePreprocess 提供（<script lang="ts"> 靠它编译）
export default {
  preprocess: vitePreprocess(),
};
