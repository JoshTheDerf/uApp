/* Locale loader. Imports every language file, picks the active one from
 * localStorage, and exports it as S. setLocale() saves the choice and
 * reloads the page so every module re-evaluates with the new strings. */

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

export function getLocale() {
  return localStorage.getItem(STORAGE_KEY) || "en";
}

export function setLocale(code) {
  if (!LANGS[code]) return;
  localStorage.setItem(STORAGE_KEY, code);
  location.reload();
}

export const S = LANGS[getLocale()] || en;
