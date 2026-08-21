//! Web permission brokering: camera, microphone, screen capture, location,
//! notifications, clipboard reads, pointer lock, and storage access.
//!
//! A .uapp is a file people hand each other, so its app code is untrusted: it
//! must not reach a capture device or the user's location just by calling the
//! matching `navigator.*` API. Every platform webview answers those calls
//! differently — WebKitGTK denies anything the embedder doesn't explicitly
//! handle (which is why an unhandled request surfaces to the page as a flat
//! "User denied", with nothing ever having asked), WebView2 shows its own
//! prompt, WKWebView via wry grants capture outright, and Android defers to the
//! OS. None of them, alone, gives a *consistent* per-app allow/deny.
//!
//! This module is that consistent layer: the native shell routes the webview's
//! permission request here, we answer from a remembered decision or ask the
//! user, and the app just sees the normal promise resolve or reject. It lives
//! in the core (no Tauri/GTK types) so the GUI-less build still compiles;
//! `gui.rs` owns the platform plumbing.
//!
//! ## Why the prompt is a native dialog, not shell UI
//!
//! It was shell UI first, and that was exploitable. The shell and the app
//! iframe share one origin, so app code can reach `window.parent`, and a
//! dynamic `import()` from the parent realm returns the *same live module
//! instances* the shell uses. Anything the shell can do — call an RPC, hold a
//! secret, answer a prompt — untrusted app code can do too. A prompt rendered
//! by the page is decorative when the page is the attacker.
//!
//! So the decision never touches JS: it is a native OS dialog, and the RPCs
//! that could grant a permission no longer exist. This is the same reason a
//! browser draws permission prompts in its chrome rather than in the document.

use serde_json::{json, Value};
use std::sync::OnceLock;

/// One thing an app can ask for. Each is remembered separately: allowing the
/// microphone must never imply the camera, and neither implies the screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Feature {
    Camera,
    Microphone,
    /// `getDisplayMedia` — deliberately distinct from `Camera`. WebKitGTK
    /// reports display capture through the *same* request type as the webcam,
    /// so without splitting these a remembered "camera: allow" would silently
    /// hand an app the whole screen.
    Screen,
    Location,
    Notifications,
    /// Reading the clipboard. Writing is not gated: a paste the user didn't
    /// ask for is annoying, a silent read is exfiltration.
    ClipboardRead,
    /// Capturing the mouse cursor (`requestPointerLock`). The mildest of these
    /// — Esc always releases it — but WebKitGTK still routes it through the
    /// permission signal, so unanswered means games silently can't grab input.
    PointerLock,
    /// `document.requestStorageAccess()`: lets an embedded third-party frame
    /// reach its own cookies. Always scoped to the requesting domain, so
    /// allowing one embed never speaks for another.
    StorageAccess,
}

impl Feature {
    /// Stable key used in prefs and on the wire. Never change these — a rename
    /// silently drops everyone's remembered decisions.
    pub fn key(self) -> &'static str {
        match self {
            Feature::Camera => "camera",
            Feature::Microphone => "microphone",
            Feature::Screen => "screen",
            Feature::Location => "location",
            Feature::Notifications => "notifications",
            Feature::ClipboardRead => "clipboard",
            Feature::PointerLock => "pointerLock",
            Feature::StorageAccess => "storage",
        }
    }

    pub fn from_key(k: &str) -> Option<Feature> {
        Some(match k {
            "camera" => Feature::Camera,
            "microphone" => Feature::Microphone,
            "screen" => Feature::Screen,
            "location" => Feature::Location,
            "notifications" => Feature::Notifications,
            "clipboard" => Feature::ClipboardRead,
            "pointerLock" => Feature::PointerLock,
            "storage" => Feature::StorageAccess,
            _ => return None,
        })
    }

    pub const ALL: &'static [Feature] = &[
        Feature::Camera,
        Feature::Microphone,
        Feature::Screen,
        Feature::Location,
        Feature::Notifications,
        Feature::ClipboardRead,
        Feature::PointerLock,
        Feature::StorageAccess,
    ];
}

/// What a single webview request covers. `getUserMedia({video, audio})` is one
/// request for two features, and must be answered as one prompt.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ask {
    pub features: Vec<Feature>,
    /// Extra context to show in the prompt — the site asking for storage
    /// access, say, which is meaningless to the user without a domain name.
    pub detail: Option<String>,
    /// Qualifies the remembered decision. Storage access uses the requesting
    /// domain, so "allow example.com its cookies" never becomes "allow every
    /// embed its cookies"; everything else is unscoped.
    pub scope: Option<String>,
}

impl Ask {
    pub fn new(features: Vec<Feature>) -> Ask {
        Ask { features, detail: None, scope: None }
    }
    pub fn one(f: Feature) -> Ask {
        Ask::new(vec![f])
    }
    /// Scope the remembered decision, and show `detail` in the prompt.
    pub fn scoped(mut self, scope: String, detail: String) -> Ask {
        self.scope = Some(scope);
        self.detail = Some(detail);
        self
    }
    pub fn is_empty(&self) -> bool {
        self.features.is_empty()
    }
    /// Stable identity for this request's feature set, for the refusal counter.
    pub fn keys_joined(&self) -> String {
        let mut k: Vec<&str> = self.features.iter().map(|f| f.key()).collect();
        k.sort_unstable();
        match &self.scope {
            Some(s) => format!("{}@{}", k.join("+"), s),
            None => k.join("+"),
        }
    }
}

/// The user's standing answer for one feature on one app.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Allow,
    Deny,
    /// No stored decision — ask.
    Ask,
}

impl Grant {
    pub fn as_str(self) -> &'static str {
        match self {
            Grant::Allow => "allow",
            Grant::Deny => "deny",
            Grant::Ask => "ask",
        }
    }
    pub fn from_str(s: &str) -> Grant {
        match s {
            "allow" => Grant::Allow,
            "deny" => Grant::Deny,
            _ => Grant::Ask,
        }
    }
}

// ---- stored decisions -------------------------------------------------
// Machine-local (prefs.json), NOT in the .uapp file: a grant is this user on
// this device trusting this app, and it must never travel to whoever they send
// the file to next.

const PREF_KEY: &str = "permissions";

/// All remembered decisions, as `{ app_id: { camera: "allow", ... } }`.
pub fn grants() -> Value {
    crate::prefs::get_value(PREF_KEY)
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

/// Where one decision is filed. Unscoped features use their bare key; scoped
/// ones (storage access) hang the scope off it, so `storage@example.com` and
/// `storage@tracker.test` are genuinely separate answers.
fn grant_key(feature: Feature, scope: Option<&str>) -> String {
    match scope {
        Some(s) if !s.is_empty() => format!("{}@{}", feature.key(), s),
        _ => feature.key().to_string(),
    }
}

/// The standing decision for one feature on one app.
pub fn grant_for(app_id: &str, feature: Feature, scope: Option<&str>) -> Grant {
    grants()
        .get(app_id)
        .and_then(|a| a.get(grant_key(feature, scope)))
        .and_then(|v| v.as_str())
        .map(Grant::from_str)
        .unwrap_or(Grant::Ask)
}

/// Remember (or with `Grant::Ask`, forget) a decision.
pub fn set_grant(
    app_id: &str,
    feature: Feature,
    scope: Option<&str>,
    g: Grant,
) -> anyhow::Result<()> {
    let mut all = grants();
    let obj = all.as_object_mut().expect("grants() returns an object");
    let entry = obj.entry(app_id.to_string()).or_insert_with(|| json!({}));
    let key = grant_key(feature, scope);
    if let Some(e) = entry.as_object_mut() {
        if g == Grant::Ask {
            e.remove(&key);
        } else {
            e.insert(key, json!(g.as_str()));
        }
    }
    // Drop apps with nothing left, so the file doesn't accumulate empty keys.
    if obj
        .get(app_id)
        .and_then(|v| v.as_object())
        .is_some_and(|o| o.is_empty())
    {
        obj.remove(app_id);
    }
    crate::prefs::set(PREF_KEY, all)
}

/// Forget every decision for one app (Settings → "Reset permissions").
pub fn clear_app(app_id: &str) -> anyhow::Result<()> {
    let mut all = grants();
    if let Some(o) = all.as_object_mut() {
        o.remove(app_id);
    }
    crate::prefs::set(PREF_KEY, all)
}

/// Every feature's standing decision for one app, for the Settings panel.
/// Scoped entries (`storage@example.com`) are surfaced verbatim alongside the
/// base features, so the panel can show and clear them individually.
pub fn summary(app_id: &str) -> Value {
    let mut out = serde_json::Map::new();
    for f in Feature::ALL {
        out.insert(f.key().to_string(), json!(grant_for(app_id, *f, None).as_str()));
    }
    if let Some(stored) = grants().get(app_id).and_then(|v| v.as_object()) {
        for (k, v) in stored {
            if k.contains('@') {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

// ---- the ask-the-user bridge ------------------------------------------
// The webview's permission request arrives on the platform UI thread, which is
// also the thread that has to paint our prompt — so the answer can never be
// produced by blocking there. `gui.rs` calls `decide()` from a worker thread
// and posts the result back; the server installs the prompt hook at startup.

/// Ask the user with a NATIVE dialog. Returns `(allow, remember)`. Blocking —
/// never call this from the UI thread.
///
/// Installed by `gui.rs`, deliberately not by the server: nothing reachable
/// from the page may take part in this decision.
type Prompt = Box<dyn Fn(Ask) -> (bool, bool) + Send + Sync>;
static PROMPT: OnceLock<Prompt> = OnceLock::new();

/// Identify the running app to the grant store. Installed by the server, which
/// is the only thing that knows which .uapp is open.
type AppIdFn = Box<dyn Fn() -> String + Send + Sync>;
static APP_ID: OnceLock<AppIdFn> = OnceLock::new();

/// Wire up the native prompt (called once by the desktop shell at startup).
pub fn install_prompt(prompt: Prompt) {
    let _ = PROMPT.set(prompt);
}

/// Wire up app identity (called once when the server starts).
pub fn install_app_id(app_id: AppIdFn) {
    let _ = APP_ID.set(app_id);
}

/// How many times one app may be refused the same thing before we stop
/// showing the dialog at all for the rest of the process.
const MAX_REFUSALS: u32 = 3;
static REFUSALS: OnceLock<std::sync::Mutex<std::collections::HashMap<String, u32>>> =
    OnceLock::new();

fn refusals(key: &str) -> u32 {
    REFUSALS
        .get_or_init(Default::default)
        .lock()
        .map(|m| m.get(key).copied().unwrap_or(0))
        .unwrap_or(0)
}

fn note_refusal(key: &str) {
    if let Ok(mut m) = REFUSALS.get_or_init(Default::default).lock() {
        *m.entry(key.to_string()).or_insert(0) += 1;
    }
}

pub fn current_app_id() -> String {
    APP_ID.get().map(|f| f()).unwrap_or_default()
}

/// True when the user has already allowed at least one of these features, with
/// no prompting. Used for device labels, which ride on an existing capture
/// grant rather than asking a question of their own.
pub fn any_allowed(features: &[Feature]) -> bool {
    let app_id = current_app_id();
    features
        .iter()
        .any(|f| grant_for(&app_id, *f, None) == Grant::Allow)
}

/// Requests whose answer is already settled, and background asks in flight.
/// A session allow covers "Allow once" for features that have to be answered
/// inline — the click that triggers the prompt cannot also be the click that
/// succeeds, so the answer has to survive until the next attempt.
static SESSION_ALLOW: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    OnceLock::new();
static ASK_IN_FLIGHT: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    OnceLock::new();

fn session_key(app_id: &str, ask: &Ask) -> String {
    format!("{}|{}", app_id, ask.keys_joined())
}

/// Answer without blocking. `Some(allow)` when the answer is already known,
/// `None` when the user still has to be asked.
///
/// For permissions that cannot tolerate a deferred grant — pointer lock is
/// validated against the focused window, and any dialog we show takes that
/// focus away — this is the only usable path.
pub fn known(ask: &Ask) -> Option<bool> {
    let app_id = current_app_id();
    let key = session_key(&app_id, ask);
    if SESSION_ALLOW
        .get_or_init(Default::default)
        .lock()
        .map(|s| s.contains(&key))
        .unwrap_or(false)
    {
        return Some(true);
    }
    let scope = ask.scope.as_deref();
    let stored: Vec<Grant> = ask
        .features
        .iter()
        .map(|f| grant_for(&app_id, *f, scope))
        .collect();
    if !stored.is_empty() && stored.iter().all(|g| *g == Grant::Allow) {
        return Some(true);
    }
    if stored.iter().any(|g| *g == Grant::Deny) {
        return Some(false);
    }
    if refusals(&key) >= MAX_REFUSALS {
        return Some(false);
    }
    None
}

/// Ask the user on a worker thread and remember the answer, so the *next*
/// attempt can be answered inline by [`known`]. The current attempt has
/// already been refused by the caller — there is no way to hold it open
/// without breaking the thing it is asking for.
///
/// Re-entrant calls while a dialog is already up are dropped: without this, a
/// feature that fails and is immediately retried stacks a dialog per attempt.
pub fn ask_in_background(ask: Ask) {
    let app_id = current_app_id();
    let key = session_key(&app_id, &ask);
    {
        let mut flight = match ASK_IN_FLIGHT.get_or_init(Default::default).lock() {
            Ok(f) => f,
            Err(_) => return,
        };
        if !flight.insert(key.clone()) {
            return; // already asking about exactly this
        }
    }
    std::thread::spawn(move || {
        let allowed = decide(ask);
        if allowed {
            if let Ok(mut s) = SESSION_ALLOW.get_or_init(Default::default).lock() {
                s.insert(key.clone());
            }
        }
        if let Ok(mut f) = ASK_IN_FLIGHT.get_or_init(Default::default).lock() {
            f.remove(&key);
        }
    });
}

/// Decide a request: stored decision first, otherwise ask the user.
///
/// Blocking, and safe to call from any thread *except* the platform UI thread
/// (the prompt is painted there). With no server attached — the GUI-less build,
/// or a shell that never connected — this denies rather than silently granting:
/// an untrusted app must not get a device because nobody was around to say no.
pub fn decide(ask: Ask) -> bool {
    if ask.is_empty() {
        return false;
    }
    let app_id = current_app_id();
    let scope = ask.scope.as_deref();

    // A request covering several features is only auto-answered when they all
    // agree; a half-remembered set goes back to the user.
    let stored: Vec<Grant> = ask
        .features
        .iter()
        .map(|f| grant_for(&app_id, *f, scope))
        .collect();
    if stored.iter().all(|g| *g == Grant::Allow) {
        return true;
    }
    if stored.iter().any(|g| *g == Grant::Deny) {
        return false;
    }

    let Some(prompt) = PROMPT.get() else {
        return false;
    };

    // Denials are not remembered (the user only said "not now"), which leaves
    // room for an app to ask over and over until someone clicks Allow to make
    // it stop. After enough refusals for the same thing, stop relaying them
    // for the rest of the session.
    let nag_key = format!("{}|{}", app_id, ask.keys_joined());
    if refusals(&nag_key) >= MAX_REFUSALS {
        return false;
    }

    let (allow, remember) = prompt(ask.clone());
    if !allow {
        note_refusal(&nag_key);
    }
    if remember && !app_id.is_empty() {
        let g = if allow { Grant::Allow } else { Grant::Deny };
        for f in &ask.features {
            let _ = set_grant(&app_id, *f, scope, g);
        }
    }
    allow
}

// ---- prompt wording -----------------------------------------------------
// The dialog is drawn by the OS, so its text has to come from here rather than
// the shell's JS locale files. The locale is read from the environment, never
// from anything the app can set — a request that says "camera" must not be
// relabelled "a harmless thing" by the code being gated.

/// Which of the shipped locales to use, from `LC_ALL`/`LC_MESSAGES`/`LANG`.
fn locale() -> &'static str {
    let raw = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if raw.starts_with("fr") {
        "fr"
    } else if raw.starts_with("es") {
        "es"
    } else if raw.starts_with("de") {
        "de"
    } else if raw.starts_with("zh") {
        "zh"
    } else {
        "en"
    }
}

/// Everything the native dialog needs to render one request.
pub struct PromptText {
    pub title: String,
    pub body: String,
    pub always_allow: &'static str,
    pub allow_once: &'static str,
    pub deny: &'static str,
}

impl Ask {
    /// The dialog's wording for this request, in the system locale.
    pub fn prompt_text(&self) -> PromptText {
        let l = locale();
        let combo: Vec<&str> = {
            let mut k: Vec<&str> = self.features.iter().map(|f| f.key()).collect();
            k.sort_unstable();
            k
        };
        let both_capture = combo == ["camera", "microphone"];
        let single = if both_capture {
            "cameraMic"
        } else if combo.len() == 1 {
            combo[0]
        } else {
            "generic"
        };

        let (title, what) = wording(l, single);
        // Pointer lock is escapable and harmless; the rest carry a warning, and
        // screen/clipboard a sharper one.
        let caution = match single {
            "pointerLock" => esc_hint(l),
            "screen" | "clipboard" => note_strong(l),
            _ => note(l),
        };
        let mut body = format!("{} {}\n\n{}", this_app(l), what, caution);
        if let Some(d) = &self.detail {
            body = format!("{} {}\n\n{}\n\n{}", this_app(l), what, d, caution);
        }
        PromptText {
            title: title.to_string(),
            body,
            always_allow: btn_always(l),
            allow_once: btn_once(l),
            deny: btn_deny(l),
        }
    }
}

fn this_app(l: &str) -> &'static str {
    match l {
        "fr" => "Cette application",
        "es" => "Esta aplicación",
        "de" => "Diese App",
        "zh" => "此应用",
        _ => "This app",
    }
}

/// `(dialog title, what the app is asking for)`.
fn wording(l: &str, key: &str) -> (&'static str, &'static str) {
    match (l, key) {
        ("fr", "camera") => ("Caméra", "demande à utiliser votre caméra."),
        ("fr", "microphone") => ("Microphone", "demande à utiliser votre microphone."),
        ("fr", "cameraMic") => ("Caméra et microphone", "demande à utiliser votre caméra et votre microphone."),
        ("fr", "screen") => ("Partage d'écran", "demande à partager votre écran."),
        ("fr", "location") => ("Localisation", "demande à connaître votre position."),
        ("fr", "notifications") => ("Notifications", "demande à afficher des notifications."),
        ("fr", "clipboard") => ("Presse-papiers", "demande à lire votre presse-papiers."),
        ("fr", "pointerLock") => ("Contrôle de la souris", "demande à capturer votre curseur."),
        ("fr", "storage") => ("Stockage intersite", "demande à laisser un site intégré utiliser ses propres cookies."),
        ("fr", _) => ("Demande d'autorisation", "demande un accès supplémentaire."),

        ("es", "camera") => ("Cámara", "quiere usar tu cámara."),
        ("es", "microphone") => ("Micrófono", "quiere usar tu micrófono."),
        ("es", "cameraMic") => ("Cámara y micrófono", "quiere usar tu cámara y tu micrófono."),
        ("es", "screen") => ("Compartir pantalla", "quiere compartir tu pantalla."),
        ("es", "location") => ("Ubicación", "quiere conocer tu ubicación."),
        ("es", "notifications") => ("Notificaciones", "quiere mostrar notificaciones."),
        ("es", "clipboard") => ("Portapapeles", "quiere leer tu portapapeles."),
        ("es", "pointerLock") => ("Control del ratón", "quiere capturar el cursor del ratón."),
        ("es", "storage") => ("Almacenamiento entre sitios", "quiere permitir que un sitio incrustado use sus propias cookies."),
        ("es", _) => ("Solicitud de permiso", "quiere acceso adicional."),

        ("de", "camera") => ("Kamera", "möchte deine Kamera verwenden."),
        ("de", "microphone") => ("Mikrofon", "möchte dein Mikrofon verwenden."),
        ("de", "cameraMic") => ("Kamera & Mikrofon", "möchte deine Kamera und dein Mikrofon verwenden."),
        ("de", "screen") => ("Bildschirmfreigabe", "möchte deinen Bildschirm teilen."),
        ("de", "location") => ("Standort", "möchte deinen Standort wissen."),
        ("de", "notifications") => ("Mitteilungen", "möchte Mitteilungen anzeigen."),
        ("de", "clipboard") => ("Zwischenablage", "möchte deine Zwischenablage lesen."),
        ("de", "pointerLock") => ("Maussteuerung", "möchte deinen Mauszeiger erfassen."),
        ("de", "storage") => ("Seitenübergreifender Speicher", "möchte einer eingebetteten Seite ihre eigenen Cookies erlauben."),
        ("de", _) => ("Berechtigungsanfrage", "möchte zusätzlichen Zugriff."),

        ("zh", "camera") => ("摄像头", "请求使用你的摄像头。"),
        ("zh", "microphone") => ("麦克风", "请求使用你的麦克风。"),
        ("zh", "cameraMic") => ("摄像头与麦克风", "请求使用你的摄像头和麦克风。"),
        ("zh", "screen") => ("屏幕共享", "请求共享你的屏幕。"),
        ("zh", "location") => ("位置", "请求获取你的位置。"),
        ("zh", "notifications") => ("通知", "请求显示通知。"),
        ("zh", "clipboard") => ("剪贴板", "请求读取你的剪贴板。"),
        ("zh", "pointerLock") => ("鼠标控制", "请求捕获你的鼠标指针。"),
        ("zh", "storage") => ("跨站点存储", "请求允许嵌入的站点使用其自身的 Cookie。"),
        ("zh", _) => ("权限请求", "请求额外的访问权限。"),

        (_, "camera") => ("Camera", "wants to use your camera."),
        (_, "microphone") => ("Microphone", "wants to use your microphone."),
        (_, "cameraMic") => ("Camera & microphone", "wants to use your camera and microphone."),
        (_, "screen") => ("Screen sharing", "wants to share your screen."),
        (_, "location") => ("Location", "wants to know your location."),
        (_, "notifications") => ("Notifications", "wants to show notifications."),
        (_, "clipboard") => ("Clipboard", "wants to read your clipboard."),
        (_, "pointerLock") => ("Mouse control", "wants to capture your mouse cursor."),
        (_, "storage") => ("Cross-site storage", "wants to let an embedded site use its own cookies."),
        (_, _) => ("Permission request", "wants additional access."),
    }
}

fn note(l: &str) -> &'static str {
    match l {
        "fr" => "Les applications sont des fichiers que l'on se partage. N'autorisez que si vous avez confiance en sa provenance.",
        "es" => "Las aplicaciones son archivos que se comparten. Permítelo solo si confías en su procedencia.",
        "de" => "Apps sind Dateien, die weitergegeben werden. Erlaube dies nur, wenn du der Quelle vertraust.",
        "zh" => "应用是可以互相传递的文件。只有在你信任其来源时才允许。",
        _ => "Apps are files people share. Only allow this if you trust where this one came from.",
    }
}

fn note_strong(l: &str) -> &'static str {
    match l {
        "fr" => "Cela peut capturer des éléments hors de cette application. N'autorisez que si vous avez confiance en sa provenance.",
        "es" => "Esto puede capturar cosas fuera de esta aplicación. Permítelo solo si confías en su procedencia.",
        "de" => "Dies kann Inhalte außerhalb dieser App erfassen. Erlaube es nur, wenn du der Quelle vertraust.",
        "zh" => "这可能会捕获此应用之外的内容。只有在你信任其来源时才允许。",
        _ => "This can capture things outside this app. Only allow it if you trust where this app came from.",
    }
}

fn esc_hint(l: &str) -> &'static str {
    match l {
        "fr" => "Vous pouvez libérer le curseur à tout moment avec Échap.",
        "es" => "Puedes liberar el cursor en cualquier momento pulsando Esc.",
        "de" => "Du kannst den Zeiger jederzeit mit Esc freigeben.",
        "zh" => "你随时可以按 Esc 释放鼠标指针。",
        _ => "You can release the cursor at any time by pressing Esc.",
    }
}

fn btn_always(l: &str) -> &'static str {
    match l {
        "fr" => "Toujours autoriser",
        "es" => "Permitir siempre",
        "de" => "Immer erlauben",
        "zh" => "始终允许",
        _ => "Always allow",
    }
}

fn btn_once(l: &str) -> &'static str {
    match l {
        "fr" => "Autoriser une fois",
        "es" => "Permitir una vez",
        "de" => "Einmal erlauben",
        "zh" => "允许一次",
        _ => "Allow once",
    }
}

fn btn_deny(l: &str) -> &'static str {
    match l {
        "fr" => "Refuser",
        "es" => "Denegar",
        "de" => "Ablehnen",
        "zh" => "拒绝",
        _ => "Deny",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_grants_do_not_speak_for_other_domains() {
        // The whole point of scoping: one embed's answer is not every embed's.
        assert_ne!(
            grant_key(Feature::StorageAccess, Some("a.example")),
            grant_key(Feature::StorageAccess, Some("b.example"))
        );
        assert_eq!(grant_key(Feature::Camera, None), "camera");
        assert_eq!(
            grant_key(Feature::StorageAccess, Some("a.example")),
            "storage@a.example"
        );
        // An empty scope must not produce a dangling "storage@".
        assert_eq!(grant_key(Feature::StorageAccess, Some("")), "storage");
    }

    #[test]
    fn every_feature_round_trips_through_its_key() {
        for f in Feature::ALL {
            assert_eq!(Feature::from_key(f.key()), Some(*f), "{}", f.key());
        }
        assert_eq!(Feature::from_key("nonsense"), None);
    }

    #[test]
    fn screen_capture_is_not_camera() {
        // A remembered camera grant must never answer a screen-share request.
        assert_ne!(Feature::Screen.key(), Feature::Camera.key());
        assert_ne!(
            grant_key(Feature::Screen, None),
            grant_key(Feature::Camera, None)
        );
    }

    #[test]
    fn prompt_names_the_thing_being_asked_for() {
        // Wording must never fall through to something vaguer than the request.
        for f in Feature::ALL {
            let t = Ask::one(*f).prompt_text();
            assert!(!t.title.is_empty(), "{}", f.key());
            assert!(t.body.contains("\n\n"), "{} body has no caution", f.key());
        }
        let both = Ask::new(vec![Feature::Camera, Feature::Microphone]).prompt_text();
        assert!(both.body.to_lowercase().contains("microphone"));
    }

    #[test]
    fn storage_prompt_carries_the_domain() {
        let a = Ask::one(Feature::StorageAccess)
            .scoped("tracker.example".into(), "tracker.example (embedded in shop.example)".into());
        assert!(a.prompt_text().body.contains("tracker.example"));
        assert_eq!(a.keys_joined(), "storage@tracker.example");
    }
}
