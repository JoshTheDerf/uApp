/* Locale loader. Imports every language file, picks the active one (the
 * choice saved in Settings, else the browser's preferred languages), and
 * exports it as S. setLocale() saves the choice and reloads the page so
 * every module re-evaluates with the new strings. */

import { S as en } from "./lang/en.js";
import { S as fr } from "./lang/fr.js";
import { S as es } from "./lang/es.js";
import { S as de } from "./lang/de.js";
import { S as zhHans } from "./lang/zh-Hans.js";

const LANGS = { en, fr, es, de, "zh-Hans": zhHans };

export const LOCALES = [
  { code: "en",      label: "English" },
  { code: "fr",      label: "Français" },
  { code: "es",      label: "Español" },
  { code: "de",      label: "Deutsch" },
  { code: "zh-Hans", label: "简体中文" },
];

const STORAGE_KEY = "uapp.lang";

/// Until someone picks a language in Settings, follow the browser's list of
/// preferred languages (the first one we have wins). Nothing is saved for
/// that, so a later change of browser language is followed too.
function detect() {
  for (const tag of navigator.languages || [navigator.language || ""]) {
    const t = String(tag).toLowerCase();
    // Simplified Chinese only: zh-TW / zh-HK / zh-Hant readers are better
    // served by the next language on their list.
    if (/^zh(-hans|-cn|-sg|-my)?(-|$)/.test(t) && !/hant|-tw|-hk|-mo/.test(t)) return "zh-Hans";
    const base = t.split("-")[0];
    if (base !== "zh" && LANGS[base]) return base;
  }
  return "en";
}

export function getLocale() {
  let saved = null;
  try { saved = localStorage.getItem(STORAGE_KEY); } catch {}
  return saved && LANGS[saved] ? saved : detect();
}

export function setLocale(code) {
  if (!LANGS[code]) return;
  try { localStorage.setItem(STORAGE_KEY, code); } catch {}
  location.reload();
}

export const S = LANGS[getLocale()] || en;

// The shell's language, for screen readers, hyphenation and fonts. On a
// hosted site the shell lives in its own root inside the site's page, whose
// <html lang> describes the site, so it goes on that root instead.
(document.getElementById("uapp-shell-root") || document.documentElement).lang = getLocale();
