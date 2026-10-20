/* Light / dark theme. "system" follows the OS (prefers-color-scheme in
 * shell.css); "light" and "dark" pin it with data-theme on <html>. A device
 * choice, so it lives in localStorage and never in the .uapp. index.html
 * applies it before first paint; this module owns it from there. */

const KEY = "uapp.theme";
export const THEMES = ["system", "light", "dark"];

export function getTheme() {
  try {
    const t = localStorage.getItem(KEY);
    return THEMES.includes(t) ? t : "system";
  } catch { return "system"; }
}

export function applyTheme(t = getTheme()) {
  const root = document.documentElement;
  if (t === "light" || t === "dark") root.dataset.theme = t;
  else delete root.dataset.theme;
}

export function setTheme(t) {
  if (!THEMES.includes(t)) return;
  try {
    if (t === "system") localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, t);
  } catch {}
  applyTheme(t);
}

applyTheme();
