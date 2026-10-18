// 主题：Tailwind v4 的 dark 变体绑在 <html class="dark"> 上，
// 所以「跟随系统」也要落到同一个 class，而不是只靠 media query。
// 系统主题变化时只有 mode === "system" 才重新求值。
import type { ThemeMode } from "../types";

const media = window.matchMedia("(prefers-color-scheme: dark)");

function resolve(mode: ThemeMode): "light" | "dark" {
  if (mode === "light" || mode === "dark") return mode;
  return media.matches ? "dark" : "light";
}

let currentMode: ThemeMode = "system";

export function applyTheme(mode: ThemeMode): void {
  currentMode = mode;
  apply(resolve(mode));
}

function apply(effective: "light" | "dark"): void {
  const root = document.documentElement;
  root.classList.toggle("dark", effective === "dark");
  root.style.colorScheme = effective;
}

// 系统切换（含浏览器缩放等触发）只在跟随系统时跟随
media.addEventListener("change", () => {
  if (currentMode === "system") apply(resolve("system"));
});

/** 当前实际呈现的主题（用于主题按钮的高亮与 Tooltip 文案）。 */
export function effectiveTheme(): "light" | "dark" {
  return resolve(currentMode);
}

/** 主题循环：跟随系统 → 浅色 → 深色 → 跟随系统。 */
export function nextTheme(mode: ThemeMode): ThemeMode {
  if (mode === "system") return "light";
  if (mode === "light") return "dark";
  return "system";
}

export const THEME_LABEL: Record<ThemeMode, string> = {
  system: "跟随系统",
  light: "浅色",
  dark: "深色",
};
