/* uapp browser build — what the page shows while it is not the shell yet,
 * or when something went wrong: the boot splash (with a question on it when
 * needed), notices at the bottom, and the full-page error card. */

import { mb } from "./env.js?v=__UAPP_BUILD__";

// ---- boot splash ------------------------------------------------------------
// index.html paints it on every load; we keep it up until the app frame is
// showing. Switching documents is a full reload and the samples are real
// downloads (the CAD one is 8 MB), so without this the page just sits there
// looking stalled after a click.

export const splashEl = document.getElementById("boot-splash");
export const splashStatus = document.getElementById("boot-status");
const splashDetail = document.getElementById("boot-detail");
const splashBar = document.getElementById("boot-bar");
let splashHidden = false;

export function splashShow(status, detail) {
  if (!splashEl) return;
  splashHidden = false;
  splashEl.style.display = "";
  splashEl.classList.remove("gone");
  if (status) splashStatus.textContent = status;
  splashBar.classList.remove("determinate");
  splashBar.style.width = "";
  splashDetail.textContent = detail || "";
}
// Detail line only — keeps whatever the status says (usually the app name).
export function splashNote(detail) {
  if (splashEl) splashDetail.textContent = detail || "";
}
export function splashProgress(loaded, total) {
  if (!splashEl) return;
  if (total && loaded <= total) {
    splashBar.classList.add("determinate");
    splashBar.style.width = Math.round((loaded / total) * 100) + "%";
    splashDetail.textContent = `${mb(loaded)} of ${mb(total)}`;
  } else {
    // No content-length (or a compressed transfer, where it undercounts):
    // back to the indeterminate bar and just report what has arrived.
    splashBar.classList.remove("determinate");
    splashBar.style.width = "";
    splashDetail.textContent = mb(loaded) + " downloaded";
  }
}
export function splashHide() {
  if (!splashEl || splashHidden) return;
  splashHidden = true;
  splashEl.classList.add("gone");
  setTimeout(() => { if (splashHidden) splashEl.style.display = "none"; }, 300);
}


// Download with a real progress bar when the server gives us a length.
export async function fetchWithProgress(url, what) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`could not fetch ${what} (HTTP ${r.status})`);
  const total = Number(r.headers.get("content-length")) || 0;
  if (!r.body || !r.body.getReader) return new Uint8Array(await r.arrayBuffer());
  const reader = r.body.getReader();
  const chunks = [];
  let loaded = 0;
  splashProgress(0, total);
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    loaded += value.length;
    splashProgress(loaded, total);
  }
  const out = new Uint8Array(loaded);
  let off = 0;
  for (const c of chunks) { out.set(c, off); off += c.length; }
  return out;
}

// A notice at the bottom of the page (one per id; showing it again replaces
// the old one). Built from text nodes: messages carry error text.
export function showBanner(id, title, text, buttons) {
  hideBanner(id);
  const el = document.createElement("div");
  el.id = id;
  el.setAttribute("role", "alert");
  el.style.cssText = "position:fixed;left:12px;right:12px;bottom:12px;margin:0 auto;max-width:520px;z-index:999998;background:#2b3145;border:1px solid #3a4260;border-radius:12px;padding:14px 16px;color:#f2f4f8;font:13px/1.55 system-ui,sans-serif;box-shadow:0 10px 30px #0007";
  const b = document.createElement("b");
  b.textContent = title;
  const p = document.createElement("div");
  p.textContent = text;
  p.style.cssText = "color:#c9cfdc;overflow-wrap:anywhere";
  const row = document.createElement("div");
  row.style.cssText = "display:flex;flex-wrap:wrap;gap:8px;margin-top:10px";
  buttons.forEach(([label, fn], i) => {
    const btn = document.createElement("button");
    btn.textContent = label;
    btn.style.cssText = "padding:7px 14px;border-radius:8px;font:inherit;font-weight:600;cursor:pointer;" +
      (i === 0 ? "border:none;background:#3f6de6;color:#fff" : "border:1px solid #4a5374;background:transparent;color:#f2f4f8");
    btn.onclick = () => fn(el);
    row.appendChild(btn);
  });
  el.append(b, p, row);
  document.body.appendChild(el);
  return el;
}
export function hideBanner(id) { const el = document.getElementById(id); if (el) el.remove(); }

// A question on the boot splash (or a plain confirm where there is none).
export function splashAsk(title, text, okLabel, cancelLabel) {
  if (!splashEl) return Promise.resolve(window.confirm(title + "\n\n" + text));
  return new Promise((resolve) => {
    splashShow(title, "");
    const track = document.getElementById("boot-track");
    if (track) track.style.display = "none";
    const box = document.createElement("div");
    box.id = "boot-ask";
    box.style.cssText = "margin-top:10px;font-size:13px;color:#4a5262";
    const p = document.createElement("p");
    p.style.margin = "0 0 14px";
    p.textContent = text;
    const row = document.createElement("div");
    row.style.cssText = "display:flex;gap:8px;justify-content:center";
    const btn = (label, primary, val) => {
      const b = document.createElement("button");
      b.textContent = label;
      b.style.cssText = "padding:8px 16px;border-radius:8px;font:inherit;font-weight:600;cursor:pointer;" +
        (primary ? "border:none;background:#2f6df6;color:#fff" : "border:1px solid #c9ced8;background:#fff;color:#1d2129");
      b.onclick = () => {
        box.remove();
        if (track) track.style.display = "";
        resolve(val);
      };
      return b;
    };
    row.append(btn(cancelLabel, false, false), btn(okLabel, true, true));
    box.append(p, row);
    splashDetail.after(box);
  });
}

// A full-page message over whatever is there (the splash, a broken shell).
// Text nodes only: the message is error text, which can carry markup.
export function errorCard(title, text, buttons) {
  const old = document.getElementById("uapp-error");
  if (old) old.remove();
  const el = document.createElement("div");
  el.id = "uapp-error";
  el.setAttribute("role", "alert");
  el.style.cssText = "position:fixed;inset:0;z-index:2147483600;display:flex;align-items:center;justify-content:center;background:#eef0f3;color:#1d2129;font:14px/1.5 system-ui,sans-serif;padding:16px";
  const card = document.createElement("div");
  card.style.cssText = "max-width:440px;text-align:center";
  const h = document.createElement("div");
  h.style.cssText = "font-size:16px;font-weight:600;margin-bottom:6px";
  h.textContent = title;
  const p = document.createElement("div");
  p.style.cssText = "color:#4a5262;overflow-wrap:anywhere;white-space:pre-line";
  p.textContent = text;
  const row = document.createElement("div");
  row.style.cssText = "display:flex;flex-wrap:wrap;gap:8px;justify-content:center;margin-top:16px";
  buttons.forEach(([label, fn], i) => {
    const b = document.createElement("button");
    b.textContent = label;
    b.style.cssText = "padding:8px 16px;border-radius:8px;font:inherit;font-weight:600;cursor:pointer;" +
      (i === 0 ? "border:none;background:#2f6df6;color:#fff" : "border:1px solid #c9ced8;background:#fff;color:#1d2129");
    b.onclick = fn;
    row.appendChild(b);
  });
  card.append(h, p, row);
  el.appendChild(card);
  document.body.appendChild(el);
  if (splashEl) splashEl.style.display = "none";
}
