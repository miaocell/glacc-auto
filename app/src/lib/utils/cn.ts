// shadcn-svelte 约定的类名合并工具：clsx 负责条件拼接，tailwind-merge 负责消解冲突。
import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}
