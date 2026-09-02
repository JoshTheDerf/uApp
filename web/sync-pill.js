/* uapp browser build — the sync pill: the state of an archive that came from a
 * URL, shown in the shell's topbar, and the Publish that sends it back.
 *
 * boot.js opened a copy of window.__uappSiteArchive (a hosted site's own
 * /site.uapp, or any ?open=<url>) and keeps this browser's edits to it. The
 * server's copy may move on (a deploy, a publish from elsewhere); only THAT
 * case gets a loud pill — local edits on top of a current server copy are just
 * the reader working, and get a quiet one. The identity is the archive's ETag
 * (read with a HEAD); the baseline is the ETag of the download this copy came
 * from (boot.js keeps it with the saved copy).
 *
 * Loaded by boot.js after the shell, in every mode that has an archive URL, so
 * the hosted-site chrome and the standalone page share one implementation.
 */

let started = false;
let serverBaseline = "";    // ETag of the copy this tab downloaded
let serverNow = "";         // latest ETag seen
let localEditAt = 0;        // ms; last change made in this tab
let pill = null;
let publishing = false;
let publishNote = "";   // last publish outcome, shown briefly in the pill
window.addEventListener("uapp-changes", () => { localEditAt = Date.now(); renderPill(); });

// ---- publish ------------------------------------------------------------------
// The local copy becomes the server's site: boot.js PUTs the export to
// /site.uapp with the site's publish token (server.rs). The token is asked
// for once per browser session and kept only in sessionStorage. A 409 means
// the server moved on since this copy was downloaded; the visitor chooses.
const TOKEN_KEY = "uapp.publish.token";
function publishToken(forget) {
  try {
    if (forget) { sessionStorage.removeItem(TOKEN_KEY); return null; }
    let t = sessionStorage.getItem(TOKEN_KEY);
    if (!t) {
      t = window.prompt("Publish token for this archive (the server's --token / UAPP_TOKEN):");
      if (t && t.trim()) sessionStorage.setItem(TOKEN_KEY, t.trim()); else return null;
    }
    return t.trim();
  } catch { return window.prompt("Publish token for this archive:"); }
}
async function publish() {
  if (publishing || !window.__uappSitePublish) return;
  const token = publishToken(false);
  if (!token) return;
  publishing = true; publishNote = "Publishing…"; renderPill();
  try {
    let r;
    try {
      r = await window.__uappSitePublish(token);
    } catch (e) {
      if (e.status === 409) {
        const go = window.confirm("The site on the server changed after your copy was downloaded. Publish anyway and replace it with your copy?");
        if (!go) throw new Error("Not published — the server copy is newer.");
        r = await window.__uappSitePublish(token, { force: true });
      } else {
        if (e.status === 403) publishToken(true);
        throw e;
      }
    }
    // The server now equals this copy: nothing local is pending.
    // A `uapp serve` site reports what changed; a library just stores the file.
    const f = r && r.result && r.result.files;
    publishNote = f ? "Published (" + (f.written || 0) + " changed, " + (f.removed || 0) + " removed)" : "Published";
    localEditAt = 0;
    serverBaseline = serverNow = (r && r.etag) || serverBaseline;
    setTimeout(() => { publishNote = ""; renderPill(); }, 6000);
  } catch (e) {
    publishNote = "Publish failed: " + String((e && e.message) || e);
    console.warn("uapp:", publishNote);
    setTimeout(() => { publishNote = ""; renderPill(); }, 10000);
  } finally {
    publishing = false;
    renderPill();
  }
}
window.__uappPublish = publish;

async function serverStamp() {
  // The archive URL may be absolute (another path on this origin).
  try {
    const r = await fetch(new URL(window.__uappSiteArchive, location.href), { method: "HEAD", cache: "no-store" });
    return r.headers.get("etag") || "";
  } catch { return ""; }
}
function ago(ms) {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return "just now";
  const m = Math.round(s / 60); if (m < 60) return m + " min ago";
  const h = Math.round(m / 60); if (h < 48) return h + " h ago";
  return Math.round(h / 24) + " days ago";
}
// Two looks: a quiet grey "Local edits · N ago · Reset" whenever this tab
// has changed its copy (the server is not newer — just a reminder that
// what you see is yours, and that a reload throws it away), and an amber
// "Server copy is newer" when the server has moved on since the download.
function renderPill() {
  const bar = document.getElementById("topbar");
  if (!bar) return;
  const stale = !!serverBaseline && !!serverNow && serverNow !== serverBaseline;
  const local = localEditAt > 0;
  if (!stale && !local && !publishNote) { if (pill) { pill.remove(); pill = null; } return; }
  if (!pill) {
    pill = document.createElement("span");
    pill.id = "site-sync-pill";
  }
  // The topbar re-renders its innerHTML (app.info, language, …) and drops
  // foreign children; put the pill back whenever it is missing.
  if (!pill.isConnected) {
    const anchor = bar.querySelector(".spacer");
    if (anchor && anchor.nextSibling) bar.insertBefore(pill, anchor.nextSibling); else bar.appendChild(pill);
  }
  pill.className = stale ? "stale" : "local";
  pill.style.cssText = stale
    ? "display:inline-flex;align-items:center;gap:8px;padding:3px 6px 3px 10px;border-radius:999px;" +
      "background:#8a6400;color:#fff;font-size:12px;line-height:1.2;white-space:nowrap"
    : "display:inline-flex;align-items:center;gap:6px;padding:1px 4px 1px 8px;border-radius:999px;" +
      "background:rgba(255,255,255,.1);color:var(--chrome-dim,#bbb);font-size:11px;line-height:1.2;white-space:nowrap";
  pill.innerHTML = "";
  const txt = document.createElement("span");
  const btn = document.createElement("button");
  btn.onclick = () => (window.__uappSiteReset ? window.__uappSiteReset() : location.reload());
  if (publishNote && !publishing) {
    txt.textContent = publishNote;
    pill.append(txt);
    if (local || stale) pill.append(publishButton(stale));
    return;
  }
  if (stale) {
    const yours = local ? "your edits " + ago(Date.now() - localEditAt) : "your copy has no edits";
    txt.textContent = "Server copy is newer (" + yours + ")";
    txt.title = "The site on the server changed after your copy was downloaded. Resetting discards the edits kept in this browser and reloads the server's version.";
    btn.textContent = "Reset to server copy";
    btn.style.cssText = "font:inherit;font-size:12px;padding:2px 8px;border-radius:999px;border:1px solid rgba(255,255,255,.6);background:rgba(255,255,255,.15);color:#fff;cursor:pointer";
  } else {
    txt.textContent = "Local edits · " + ago(Date.now() - localEditAt);
    txt.title = "You are working on your own copy of the site, kept in this browser across reloads. Nothing is sent to the server.";
    btn.textContent = "Reset";
    btn.title = "Discard your local copy and return to the server's version";
    btn.style.cssText = "font:inherit;font-size:11px;padding:1px 6px;border-radius:999px;border:1px solid rgba(255,255,255,.25);background:transparent;color:inherit;cursor:pointer";
  }
  pill.append(txt, btn);
  if (local) pill.append(publishButton(stale));
}
function publishButton(stale) {
  const b = document.createElement("button");
  b.id = "site-publish-btn";
  b.textContent = publishing ? "Publishing…" : "Publish";
  b.disabled = publishing;
  b.title = "Make your copy the site everyone sees (needs the site's publish token)";
  b.onclick = publish;
  b.style.cssText = stale
    ? "font:inherit;font-size:12px;padding:2px 8px;border-radius:999px;border:1px solid rgba(255,255,255,.6);background:#fff;color:#8a6400;font-weight:600;cursor:pointer"
    : "font:inherit;font-size:11px;padding:1px 8px;border-radius:999px;border:1px solid rgba(255,255,255,.25);background:#3f6de6;color:#fff;font-weight:600;cursor:pointer";
  return b;
}
async function checkServer() {
  const st = await serverStamp();
  if (!st) return;
  if (!serverBaseline) serverBaseline = st;
  serverNow = st;
  renderPill();
}
window.__uappSyncState = () => ({ serverBaseline, serverNow, localEditAt, pill: !!pill });
export function startSyncWatch() {
  if (started || !window.__uappSiteArchive) return;
  started = true;
  // boot.js has the copy open by now: its download stamp is the baseline
  // (persisted with the saved copy, so it survives reloads), and a saved
  // copy's last save time is when "your edits" happened.
  const st = window.__uappSiteState;
  if (st) {
    serverBaseline = st.etag || "";
    if (st.hasLocal && st.localSavedAt) localEditAt = st.localSavedAt;
  }
  renderPill();
  const bar = document.getElementById("topbar");
  if (bar) new MutationObserver(() => { if (pill && !pill.isConnected) renderPill(); }).observe(bar, { childList: true });
  checkServer();
  setInterval(checkServer, 60000);
  setInterval(() => { if (pill) renderPill(); }, 30000);
  document.addEventListener("visibilitychange", () => { if (!document.hidden) checkServer(); });
  window.addEventListener("focus", checkServer);
}
