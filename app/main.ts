import { mount } from "svelte";
import App from "./App.svelte";
import "./styles/global.css";

// 屏蔽 WebView 自带的右键菜单（刷新 / 打印 / 检查等）：桌面应用不需要浏览器菜单。
// 监听在 window 冒泡阶段，不阻止传播，组件自己的右键交互（首页账号卡菜单）照常工作。
window.addEventListener("contextmenu", (e) => e.preventDefault());

const app = mount(App, {
  target: document.getElementById("app")!,
});

export default app;
