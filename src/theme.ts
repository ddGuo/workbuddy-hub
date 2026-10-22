/**
 * 主题：跟随系统 / 浅色 / 深色。
 *
 * 用 `data-theme` 挂在 `<html>` 上，样式表按属性覆盖变量。
 * 选「跟随系统」时不写死结果，只监听 `prefers-color-scheme`，
 * 系统在日落时切夜间模式，界面跟着变。
 */

export type ThemeMode = 'system' | 'light' | 'dark';

const KEY = 'hub.theme';

export function loadThemeMode(): ThemeMode {
  try {
    // 调试用：?theme=light|dark 只在本次加载生效，不会写回 localStorage
    const q = new URLSearchParams(window.location.search).get('theme');
    if (q === 'light' || q === 'dark') return q;
  } catch {
    /* 忽略 */
  }
  try {
    const v = localStorage.getItem(KEY);
    if (v === 'light' || v === 'dark' || v === 'system') return v;
  } catch {
    /* 隐私模式下 localStorage 可能不可用 */
  }
  return 'system';
}

export function saveThemeMode(mode: ThemeMode) {
  try {
    localStorage.setItem(KEY, mode);
  } catch {
    /* 忽略 */
  }
}

export function systemTheme(): 'light' | 'dark' {
  if (typeof window === 'undefined' || !window.matchMedia) return 'dark';
  return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
}

export function resolveTheme(mode: ThemeMode): 'light' | 'dark' {
  return mode === 'system' ? systemTheme() : mode;
}

export function applyTheme(mode: ThemeMode) {
  const resolved = resolveTheme(mode);
  const root = document.documentElement;
  root.dataset.theme = resolved;
  root.dataset.themeMode = mode;
  root.style.colorScheme = resolved;
}
