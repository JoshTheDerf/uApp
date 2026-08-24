/* <settings-panel> — AI provider config (stored in the file), the local
 * open-in preference, launcher install/remove (with a custom icon), encryption
 * at rest, and file maintenance. An overlay side panel like Files/SQL/Chat. */

import { rpc, state, refreshInfo } from "./core.js";
import { mirrorRefresh, mirrorSupported } from "./mirror.js";
import {
  $, I, esc, installStyle, registerPanel, closePanel,
  dlgAlert, dlgConfirm, wirePwToggle, downloadUrl, fmtSize,
} from "./ui.js";
import { S, LOCALES, getLocale, setLocale } from "./strings.js";
import { pickTemplate } from "./template-update.js";

const TPL = /* html */ `
  <div class="panel-head">
    <span class="panel-title"><span id="st-head-icon"></span>${S.settings.title}</span>
    <div class="spacer"></div>
    <button id="btn-settings-close" class="icon-btn" title="${S.common.closeEsc}"></button>
  </div>
  <div class="panel-body" id="settings-form">
    <div id="export-box">
      <div class="sect-head"><b>${S.settings.downloadSection}</b></div>
      <p class="hint">${S.settings.downloadHint}</p>
      <div class="row">
        <button id="btn-dl-app" class="btn primary sm"></button>
        <button id="btn-dl-template" class="btn sm"></button>
        <button id="btn-update-template" class="btn sm"></button>
      </div>
      <p class="hint">${S.settings.updateHint}</p>
    </div>
    <hr>
    <!-- Desktop only: open a DIFFERENT .uapp (the topbar offers the same
         thing on a blank app). The way in when .uapp files aren't associated
         with UApp on this machine, so there's nothing to double-click. -->
    <div id="open-box" class="hidden">
      <div class="sect-head"><b>${S.settings.openSection}</b></div>
      <p class="hint">${S.settings.openHint}</p>
      <div class="row">
        <button id="btn-open-uapp" class="btn sm"></button>
      </div>
    </div>
    <hr id="open-hr" class="hidden">
    <p class="hint">${S.settings.aiHint}</p>
    <label>${S.settings.aiProvider}
      <select id="cfg-provider">
        <option value="">${S.settings.providerNone}</option>
        <option value="anthropic">${S.settings.providerAnthropic}</option>
        <option value="zai">${S.settings.providerZai}</option>
        <option value="openrouter">${S.settings.providerOpenRouter}</option>
        <option value="openai">${S.settings.providerOpenAI}</option>
        <option value="local">${S.settings.providerLocal}</option>
      </select>
    </label>
    <p class="hint hidden" id="local-hint">${S.settings.localHint}</p>
    <label>${S.settings.apiKey}
      <span class="pwfield"><input id="cfg-key" type="password" autocomplete="off"><button type="button" class="icon-btn pw-toggle" title="Show / hide"></button></span>
    </label>
    <label>${S.settings.model}
      <input id="cfg-model" placeholder="${S.settings.modelPlaceholder}">
    </label>
    <label>${S.settings.baseUrl}
      <input id="cfg-baseurl" placeholder="${S.settings.baseUrlPlaceholder}">
    </label>
    <label>${S.settings.maxTokens}
      <input id="cfg-maxtokens" type="number" min="1024" placeholder="${S.settings.maxTokensPlaceholder}">
    </label>
    <p class="hint">${S.settings.mcpHint}</p>
    <hr>
    <label>${S.settings.language}
      <select id="cfg-lang">
        ${LOCALES.map((l) => `<option value="${l.code}"${l.code === getLocale() ? " selected" : ""}>${l.label}</option>`).join("")}
      </select>
    </label>
    <p class="hint">${S.settings.languageHint}</p>
    <hr>
    <label>${S.settings.openIn}
      <select id="cfg-shell">
        <option value="native">${S.settings.openNative}</option>
        <option value="browser">${S.settings.openBrowser}</option>
      </select>
    </label>
    <p class="hint">${S.settings.openInHint}</p>
    <hr>
    <div id="launch-box">
      <div class="sect-head"><b>${S.settings.launcherSection}</b><span id="launch-status" class="sect-badge"></span></div>
      <p class="hint">${S.settings.launcherHint}</p>
      <div id="launch-fields"></div>
      <div id="launch-msg" class="hint"></div>
      <input type="file" id="launch-icon-file" accept="image/png" class="hidden">
    </div>
    <hr>
    <!-- Android only: an intent-opened doc works on a private copy; link the
         original .uapp so every change is written back to it. -->
    <div id="mirror-box" class="hidden">
      <div class="sect-head"><b>${S.settings.originalFileSection}</b><span id="mirror-status" class="sect-badge"></span></div>
      <p class="hint">${S.settings.originalFileHint}</p>
      <div class="row" id="mirror-fields"></div>
    </div>
    <hr id="mirror-hr" class="hidden">
    <div id="crypt-box">
      <div class="sect-head"><b>${S.settings.encryptionSection}</b><span id="crypt-status" class="sect-badge"></span></div>
      <p class="hint">${S.settings.encryptionHint}</p>
      <div id="crypt-fields"></div>
      <div id="crypt-msg" class="hint"></div>
    </div>
    <details>
      <summary>${S.settings.maintenance}</summary>
      <div class="row">
        <button id="btn-compact" class="btn sm">${S.settings.compactNow}</button>
        <span id="sync-info" class="hint"></span>
      </div>
    </details>
  </div>
  <div class="panel-foot">
    <button id="btn-cancel" class="btn">${S.common.cancel}</button>
    <button id="btn-save" class="btn primary">${S.common.save}</button>
  </div>
`;

const CSS = /* css */ `
#settingspanel { width: min(440px, 94vw); background: var(--surface); }
.sect-head { display: flex; align-items: center; gap: 8px; }
.sect-badge { font-size: 10px; border-radius: 999px; padding: 2px 9px; font-weight: 600; }
.sect-badge.on { background: var(--ok-soft); color: var(--ok-ink); }
.sect-badge.off { background: var(--hover); color: var(--muted); }
#crypt-fields, #launch-fields { display: flex; flex-direction: column; gap: 8px; margin: 8px 0; }
#crypt-fields .btn { align-self: flex-start; }
#launch-fields .row { flex-wrap: wrap; margin-top: 0; }
#export-box .row { flex-wrap: wrap; }
#launch-icon-preview { width: 22px; height: 22px; border-radius: 5px; object-fit: contain; }
#settings-form details summary { cursor: pointer; font-size: 13px; color: var(--text-2); }
`;

// ---------- launcher (applications menu / Start Menu / home screen) ----------
// Desktop: the server creates/removes the real launcher entry (RPCs below),
// never leaving duplicates behind. Android native webview: the MainActivity
// bridge pins a launcher shortcut. Mobile browser: manual hint.
let androidBridge = null; // polled: injected once MainActivity finds the webview
const mobileUA = /android|iphone|ipad|ipod|mobile/i.test(navigator.userAgent);
let iconB64 = null, iconName = "";

function launchBtn(label, cls) {
  const b = document.createElement("button");
  b.className = "btn sm" + (cls ? " " + cls : "");
  b.textContent = label;
  return b;
}
/// Icon picker row (choose / preview / clear) — shared by the desktop RPC
/// install and the Android bridge install.
function iconPickerRow() {
  const row = document.createElement("div");
  row.className = "row";
  const pick = launchBtn(iconName ? S.settings.iconChosen(iconName) : S.settings.chooseIcon);
  pick.onclick = () => $("launch-icon-file").click();
  row.appendChild(pick);
  if (iconB64) {
    const img = document.createElement("img");
    img.id = "launch-icon-preview";
    img.src = "data:image/png;base64," + iconB64;
    row.appendChild(img);
    const clr = launchBtn("✕");
    clr.title = S.settings.useDefaultIcon;
    clr.onclick = () => { iconB64 = null; iconName = ""; renderLauncher(); };
    row.appendChild(clr);
  }
  return row;
}
async function renderLauncher() {
  const fields = $("launch-fields");
  const badge = $("launch-status");
  const msg = $("launch-msg");
  fields.innerHTML = "";
  msg.textContent = "";
  badge.textContent = "";
  badge.className = "sect-badge";
  const info = state.info || {};
  // 1. Native Android: the bridge pins a real launcher shortcut (with the
  //    custom icon when the app build supports it — older builds only have
  //    the two-arg method).
  if (androidBridge && info.path) {
    fields.appendChild(iconPickerRow());
    const b = launchBtn(S.settings.addToLauncher, "primary");
    b.onclick = () => {
      const nm = (info.name || "uapp").replace(/"/g, "'");
      try {
        if (iconB64 && typeof androidBridge.addShortcutIcon === "function") {
          androidBridge.addShortcutIcon(nm, info.path, iconB64);
        } else {
          androidBridge.addShortcut(nm, info.path);
        }
      } catch (e) { msg.textContent = "Failed: " + e.message; }
    };
    fields.appendChild(b);
    return;
  }
  // 2. Mobile browser without the bridge: manual instructions.
  if (mobileUA && !info.native) {
    msg.textContent = S.settings.mobileLauncherHint;
    return;
  }
  // 3. Desktop: ask the server.
  let st;
  try { st = await rpc("app.shortcutStatus"); }
  catch (e) { msg.textContent = e.message; return; }
  if (!st.supported) {
    badge.textContent = S.settings.launcherUnavailable;
    badge.classList.add("off");
    msg.textContent = st.detail || S.settings.launcherNeedsSave;
    return;
  }
  badge.textContent = st.installed ? S.settings.launcherInstalled : S.settings.launcherNotInstalled;
  badge.classList.add(st.installed ? "on" : "off");
  if (st.installed && st.detail) msg.textContent = st.detail;

  // Icon picker (used by Install and Reinstall alike; PNG, optional).
  fields.appendChild(iconPickerRow());

  const actions = document.createElement("div");
  actions.className = "row";
  const doInstall = async (btn) => {
    btn.disabled = true;
    try {
      const r = await rpc("app.installShortcut", iconB64 ? { icon_b64: iconB64 } : {});
      msg.textContent = (r && r.message) || "Installed.";
      await renderLauncher();
    } catch (e) { btn.disabled = false; msg.textContent = e.message; }
  };
  if (!st.installed) {
    const inst = launchBtn(S.settings.installLauncher, "primary");
    inst.onclick = () => doInstall(inst);
    actions.appendChild(inst);
  } else {
    // Re-install updates name/target/icon in place — never a second entry.
    const re = launchBtn(S.settings.reinstall);
    re.onclick = () => doInstall(re);
    actions.appendChild(re);
    const rm = launchBtn(S.settings.remove, "danger");
    rm.onclick = async () => {
      if (!(await dlgConfirm(S.settings.removeLauncherConfirm))) return;
      rm.disabled = true;
      try {
        const r = await rpc("app.removeShortcut");
        msg.textContent = (r && r.message) || "Removed.";
        await renderLauncher();
      } catch (e) { rm.disabled = false; msg.textContent = e.message; }
    };
    actions.appendChild(rm);
  }
  fields.appendChild(actions);
}

// ---------- encryption ----------
const mkPw = (ph) => {
  const wrap = document.createElement("span");
  wrap.className = "pwfield";
  const i = document.createElement("input");
  i.type = "password"; i.placeholder = ph; i.autocomplete = "new-password";
  const t = document.createElement("button");
  t.type = "button"; t.className = "icon-btn pw-toggle"; t.title = "Show / hide";
  wrap.append(i, t);
  wirePwToggle(wrap);
  // doCrypt reads .value off what mkPw returns — forward it to the input.
  Object.defineProperty(wrap, "value", { get: () => i.value });
  return wrap;
};
const mkBtn = (label, variant) => {
  const b = document.createElement("button");
  b.className = "btn sm" + (variant === "brand" ? " primary" : variant === "danger" ? " danger" : "");
  b.textContent = label;
  return b;
};
async function doCrypt(fields, current, p1, p2, removing) {
  const msg = $("crypt-msg");
  const newpw = removing ? "" : p1.value;
  if (!removing && newpw !== p2.value) {
    msg.textContent = S.settings.passwordMismatch; msg.className = "hint err"; return;
  }
  msg.textContent = S.settings.encryptWorking; msg.className = "hint";
  fields.querySelectorAll("button, input").forEach((el) => (el.disabled = true));
  try {
    const r = await rpc("crypt.set", {
      current_password: current ? current.value : "",
      new_password: newpw,
    });
    msg.textContent = r.encrypted
      ? S.settings.encryptDone
      : S.settings.decryptDone;
    msg.className = "hint ok";
    await renderCrypt();
  } catch (e) {
    fields.querySelectorAll("button, input").forEach((el) => (el.disabled = false));
    msg.textContent = e.message; msg.className = "hint err";
  }
}
async function renderCrypt() {
  let enc = false;
  try { enc = !!(await rpc("app.info")).encrypted; } catch {}
  const badge = $("crypt-status");
  badge.textContent = enc ? S.settings.encrypted : S.settings.notEncrypted;
  badge.className = "sect-badge " + (enc ? "on" : "off");
  const fields = $("crypt-fields");
  fields.innerHTML = "";
  $("crypt-msg").textContent = "";
  if (!enc) {
    const p1 = mkPw(S.settings.newPassword), p2 = mkPw(S.settings.confirmPassword);
    const go = mkBtn(S.settings.encryptBtn, "brand");
    go.onclick = () => doCrypt(fields, null, p1, p2, false);
    fields.append(p1, p2, go);
  } else {
    const cur = mkPw(S.settings.currentPassword);
    const p1 = mkPw(S.settings.newPasswordChange), p2 = mkPw(S.settings.confirmPasswordChange);
    const change = mkBtn(S.settings.changePassword, "brand");
    change.onclick = () => doCrypt(fields, cur, p1, p2, false);
    const remove = mkBtn(S.settings.removeEncryption, "danger");
    remove.onclick = async () => {
      if (!(await dlgConfirm(S.settings.removeEncryptionConfirm))) return;
      doCrypt(fields, cur, p1, p2, true);
    };
    fields.append(cur, p1, p2, change, remove);
  }
}

// ---------- open / save ----------
function syncLocalHint() {
  $("local-hint").classList.toggle("hidden", $("cfg-provider").value !== "local");
}
async function loadSettings() {
  const ai = (await rpc("config.get", { key: "ai" })) || {};
  $("cfg-provider").value = ai.provider || "";
  $("cfg-key").value = ai.api_key || "";
  $("cfg-model").value = ai.model || "";
  $("cfg-baseurl").value = ai.base_url || "";
  $("cfg-maxtokens").value = ai.max_tokens || "";
  syncLocalHint();
  try {
    const prefs = (await rpc("prefs.get")) || {};
    // Native is the default; "browser" is the explicit opt-out.
    $("cfg-shell").value = prefs.shell === "browser" ? "browser" : "native";
  } catch {}
  try {
    const info = await rpc("app.info");
    $("sync-info").textContent = S.settings.fileSize(fmtSize(info.fileSize || 0));
  } catch {}
  renderLauncher();
  renderMirror();
  renderCrypt();
  renderOpenAnother();
}

// "Open another app" is a native-desktop trick: it shows an OS file dialog and
// starts a second instance. Mobile and the browser shell can't do either, so
// the whole section stays hidden there.
function renderOpenAnother() {
  const info = state.info || {};
  const usable = !!info.native && info.desktop !== false && !info.wasm;
  $("open-box").classList.toggle("hidden", !usable);
  $("open-hr").classList.toggle("hidden", !usable);
}

// ---------- original-file write-back (Android intent-opened docs) ----------
function renderMirror() {
  const info = state.info;
  const b = window.UAppAndroid;
  // Only offered where it makes sense: the Android app, a doc that has an
  // original somewhere (not the unsaved default doc), and an APK with the
  // bridge. mirror.js does the actual mirroring.
  const usable = mirrorSupported() && typeof b.mirrorStatus === "function"
    && info && info.path && !info.unsaved;
  $("mirror-box").classList.toggle("hidden", !usable);
  $("mirror-hr").classList.toggle("hidden", !usable);
  if (!usable) return;
  const linked = mirrorRefresh(info.path);
  const badge = $("mirror-status");
  badge.textContent = linked ? S.settings.mirrorLinked : S.settings.mirrorNotLinked;
  badge.className = "sect-badge " + (linked ? "on" : "off");
  const row = $("mirror-fields");
  row.innerHTML = "";
  const btn = document.createElement("button");
  btn.className = "btn sm" + (linked ? "" : " primary");
  btn.textContent = linked ? S.settings.mirrorUnlink : S.settings.mirrorLink;
  btn.onclick = () => {
    if (linked) {
      b.unlinkOriginal(info.path);
      renderMirror();
      return;
    }
    b.linkOriginal(info.path); // opens the system picker
    // Poll for the pick result (there's no callback across the bridge).
    let tries = 0;
    const t = setInterval(() => {
      if (mirrorRefresh(info.path) || ++tries > 90) {
        clearInterval(t);
        renderMirror();
      }
    }, 1000);
  };
  row.appendChild(btn);
  // Optional "All files access" upgrade: with it granted, tapped documents
  // link automatically even when the file manager only grants read access.
  if (typeof b.allFilesAccess === "function" && b.allFilesAccess() === "denied") {
    const hint = document.createElement("p");
    hint.className = "hint";
    hint.textContent = S.settings.allFilesHint;
    const afBtn = document.createElement("button");
    afBtn.className = "btn sm";
    afBtn.textContent = S.settings.allFilesBtn;
    afBtn.onclick = async () => {
      // Prompt first: say what the permission does and where the tap leads,
      // so the system Settings jump isn't disorienting.
      if (!(await dlgConfirm(S.settings.allFilesConfirm))) return;
      b.requestAllFilesAccess(); // jumps to the system Settings toggle
      // Poll until the user returns with the grant (timers freeze while the
      // app is backgrounded, so this resumes exactly when they come back).
      let tries = 0;
      const t = setInterval(() => {
        if (b.allFilesAccess() === "granted") {
          clearInterval(t);
          if (typeof b.retryAutoLink === "function") b.retryAutoLink(info.path);
          setTimeout(renderMirror, 300);
        } else if (++tries > 120) clearInterval(t);
      }, 1000);
    };
    row.appendChild(afBtn);
    row.parentElement.appendChild(hint);
  }
}

// ---------- component ----------
class SettingsPanel extends HTMLElement {
  connectedCallback() {
    installStyle("settings-panel", CSS);
    this.innerHTML = TPL;
    wire();
  }
}

function wire() {
  registerPanel("settingspanel", "btn-settings", loadSettings);
  $("st-head-icon").innerHTML = I("settings", 14);
  $("btn-settings-close").innerHTML = I("x", 15);
  $("btn-settings-close").onclick = () => closePanel("settingspanel");
  $("btn-dl-app").innerHTML = I("download", 13) + " " + S.settings.downloadApp;
  $("btn-dl-template").innerHTML = I("package", 13) + " " + S.settings.downloadTemplate;
  $("btn-dl-app").onclick = () => downloadUrl("/download.uapp");
  $("btn-dl-template").onclick = () => downloadUrl("/template.uapp");
  $("btn-update-template").innerHTML = I("folder-input", 13) + " " + S.settings.loadUpdate;
  $("btn-update-template").onclick = () => pickTemplate();
  // Open another .uapp in its own window (see the app.openFile RPC): a native
  // file dialog, then that app starts on its own — this one keeps running.
  $("btn-open-uapp").innerHTML = I("folder-open", 13) + " " + S.settings.openApp;
  $("btn-open-uapp").onclick = async () => {
    const b = $("btn-open-uapp");
    b.disabled = true;
    try {
      const r = await rpc("app.openFile");
      if (r && r.opened) closePanel("settingspanel"); // the new window has it
    } catch (e) {
      dlgAlert(S.settings.openFailed(e.message));
    } finally { b.disabled = false; }
  };
  wirePwToggle(document.querySelector("#settings-form .pwfield"));

  $("launch-icon-file").addEventListener("change", (e) => {
    const f = e.target.files[0];
    e.target.value = "";
    if (!f) return;
    if (f.size > 512 * 1024) { dlgAlert(S.settings.iconTooLarge); return; }
    const rd = new FileReader();
    rd.onload = () => {
      iconB64 = rd.result.split(",", 2)[1];
      iconName = f.name;
      renderLauncher();
    };
    rd.readAsDataURL(f);
  });

  // Poll briefly for the Android bridge (injected a moment after startup).
  let bridgeTries = 0;
  const bridgeTimer = setInterval(() => {
    if (window.UAppAndroid && window.UAppAndroid.addShortcut) {
      androidBridge = window.UAppAndroid;
      clearInterval(bridgeTimer);
    } else if (++bridgeTries > 20) clearInterval(bridgeTimer);
  }, 500);

  $("cfg-lang").onchange = () => setLocale($("cfg-lang").value);
  $("cfg-provider").onchange = syncLocalHint;
  $("btn-cancel").onclick = () => closePanel("settingspanel");
  $("btn-save").onclick = async () => {
    const provider = $("cfg-provider").value;
    const ai = provider ? {
      provider,
      api_key: ($("cfg-key").value || "").trim(),
      model: ($("cfg-model").value || "").trim(),
      base_url: ($("cfg-baseurl").value || "").trim() || undefined,
      max_tokens: parseInt($("cfg-maxtokens").value, 10) || undefined,
    } : null;
    try {
      await rpc("config.set", { key: "ai", value: ai });
      // Local (not synced) — how this machine opens apps.
      await rpc("prefs.set", { key: "shell", value: $("cfg-shell").value }).catch(() => {});
    } catch (e) { dlgAlert(S.settings.saveFailed(e.message)); return; }
    closePanel("settingspanel");
    refreshInfo();
  };
  $("btn-compact").onclick = async () => {
    const r = await rpc("app.compact").catch((e) => { dlgAlert(e.message); return null; });
    if (r) $("sync-info").textContent = S.settings.fileSize(fmtSize(r.fileSize || 0));
  };
}

customElements.define("settings-panel", SettingsPanel);
