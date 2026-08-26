package com.thederf.uapp

import android.app.DownloadManager
import android.content.Intent
import android.graphics.Color
import android.graphics.drawable.ColorDrawable
import android.net.Uri
import android.os.Bundle
import android.util.Log
import android.view.View
import android.view.ViewGroup
import android.webkit.CookieManager
import android.webkit.JavascriptInterface
import android.webkit.WebView
import android.widget.Toast
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge
import androidx.core.content.pm.ShortcutInfoCompat
import androidx.core.content.pm.ShortcutManagerCompat
import androidx.core.graphics.drawable.IconCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import androidx.webkit.WebViewRenderProcess
import androidx.webkit.WebViewRenderProcessClient
import java.io.File

// gen/android is a committed project (the standard Tauri v2 workflow): this
// file IS the source of truth, edited in place like the manifest and icons.
class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    // Edge-to-edge is enforced on Android 15+ (targetSdk 35+) regardless, so
    // opt in everywhere for identical behavior on older devices. Both bars sit
    // on the dark chrome color below, so request light (dark-background) style
    // icons for readable status-bar content.
    enableEdgeToEdge(
      statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
      navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT)
    )
    // Stage intent-delivered documents BEFORE super.onCreate(): WryActivity's
    // onCreate starts the Rust core synchronously (Rust.create() via its
    // lifecycle observer), and the core resolves which .uapp to open exactly
    // once, at startup (resolve_app_path in gui.rs). Staging after super()
    // loses the race — the core has already opened the default document by
    // the time the marker files exist, so "open with" appears to be ignored
    // on every cold start. Context (filesDir/contentResolver) is attached in
    // attachBaseContext, well before onCreate, so this is safe here.
    // Older builds staged the markers into filesDir, where the core never
    // read them — drop any that are still lying around so the core's files/
    // fallback doesn't surprise-open a long-ago-tapped document.
    File(filesDir, PENDING_DOC).delete()
    File(filesDir, PENDING_NAME).delete()
    File(filesDir, SHORTCUT_PATH).delete()
    handleViewIntent(intent)
    handleShortcutIntent(intent)

    super.onCreate(savedInstanceState)

    // Edge-to-edge means the webview is laid out under the status bar / camera
    // cutout and the navigation bar. Pad the content view by the system insets
    // (including the IME so the keyboard never covers the page) so the app
    // content always sits clear of them. Padding must go on the parent view —
    // WebView itself ignores padding. Re-fires on rotation, keyboard and
    // gesture/button-nav changes.
    val content = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(content) { view, windowInsets ->
      val bars = windowInsets.getInsets(
        WindowInsetsCompat.Type.systemBars()
          or WindowInsetsCompat.Type.displayCutout()
          or WindowInsetsCompat.Type.ime()
      )
      view.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      WindowInsetsCompat.CONSUMED
    }

    // The padded strips show the window background; match the shell's dark
    // topbar (#23293a in src/shell/shell.css) so the status-bar area reads as
    // app chrome instead of a foreign band.
    window.setBackgroundDrawable(ColorDrawable(CHROME_COLOR))

    // Hook the WebView as soon as Tauri builds it (some time after onCreate).
    hookWebView()
  }

  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    // The server is already running with another document; the only clean way
    // to switch is a process restart (the core is file-backed, so this is
    // safe). Relaunch, then exit — the new process picks up the marker file.
    if (handleViewIntent(intent) || handleShortcutIntent(intent)) {
      val relaunch = Intent(this, MainActivity::class.java)
      relaunch.flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK
      startActivity(relaunch)
      Runtime.getRuntime().exit(0)
    }
  }

  // A WebView only exists once Tauri has built it, some time after onCreate.
  // Poll until it appears: addJavascriptInterface only takes effect on the
  // next page (re)load, so the bridge must be attached before the initial
  // navigation commits — waiting for the next onResume would be too late.
  private var downloadHooked = false
  private fun hookWebView() {
    if (downloadHooked) return
    val content = findViewById<View>(android.R.id.content)
    val wv = findWebView(content) ?: run {
      content.postDelayed({ hookWebView() }, 50)
      return
    }
    wv.setDownloadListener { url, _, contentDisposition, mimeType, _ ->
      enqueueDownload(url, contentDisposition, mimeType)
    }
    // Expose the install/save bridge to the shell (see ShellBridge below).
    wv.addJavascriptInterface(ShellBridge(), "UAppAndroid")
    downloadHooked = true
  }

  override fun onResume() {
    super.onResume()
    hookWebView() // belt-and-braces if the poll was somehow lost
  }

  // ---- write-back to a linked original .uapp (SAF) ----
  // An intent-opened document is imported into a private working copy (SAF
  // gives no durable path for SQLite). Write-back to the original needs a
  // WRITE grant on its URI, established in one of three ways, best first:
  //  1. The VIEW intent's grant is persistable (rare — the sender must set
  //     FLAG_GRANT_PERSISTABLE_URI_PERMISSION): auto-linked durably.
  //  2. The VIEW intent carries a transient write grant: auto-linked for
  //     this process's lifetime only (sessionMirror).
  //  3. Read-only grant (the common "open with" case): the user re-picks the
  //     file once via Settings → Original file (ACTION_OPEN_DOCUMENT is the
  //     only channel Android guarantees a persistable read/write grant on).
  // The shell mirrors every change to the linked URI via its /download.uapp
  // export — always a consistent snapshot, never raw bytes of the live db.
  private val mirrorPrefs by lazy { getSharedPreferences("uapp_mirror", MODE_PRIVATE) }
  // The intent URI each working copy was imported from (see handleViewIntent).
  private val sourcePrefs by lazy { getSharedPreferences("uapp_source", MODE_PRIVATE) }
  private var pendingLinkPath: String? = null
  private val pickOriginal = registerForActivityResult(
    androidx.activity.result.contract.ActivityResultContracts.OpenDocument()
  ) { uri -> onOriginalPicked(uri) }

  private fun onOriginalPicked(uri: Uri?) {
    val path = pendingLinkPath ?: return
    pendingLinkPath = null
    if (uri == null) return // picker cancelled
    try {
      contentResolver.takePersistableUriPermission(
        uri,
        Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
      )
      mirrorPrefs.edit().putString(path, uri.toString()).apply()
      // Warn (but proceed) when the picked file's name doesn't match — the
      // user may have picked the wrong document, and it WILL be overwritten.
      val picked = queryDisplayName(uri)
      val expect = File(path).name
      val msg = if (picked != null && picked != expect) {
        "Linked \"$picked\" — it will be overwritten with this app's content"
      } else {
        "Linked — changes now write back to the original file"
      }
      Toast.makeText(this, msg, Toast.LENGTH_LONG).show()
      flushMirror() // bring the original up to date right away
    } catch (e: Exception) {
      Toast.makeText(this, "Could not link: ${e.message}", Toast.LENGTH_LONG).show()
    }
  }

  private fun queryDisplayName(uri: Uri): String? = try {
    contentResolver.query(
      uri, arrayOf(android.provider.OpenableColumns.DISPLAY_NAME), null, null, null
    )?.use { if (it.moveToFirst()) it.getString(0) else null }
  } catch (e: Exception) { null }

  /// The linked URI for a working path: a persisted link first (dropping it
  /// when its grant has been revoked), else a session link from autoLink.
  private fun linkedUri(path: String): Uri? {
    val s = mirrorPrefs.getString(path, null)
    if (s != null) {
      val uri = Uri.parse(s)
      if (uri.scheme == "file") {
        // Raw-path link from all-files access. Usable only while the
        // permission is granted; KEPT (not deleted) when revoked, so it
        // comes back if the user re-grants.
        if (hasAllFilesAccess()) return uri
      } else {
        if (contentResolver.persistedUriPermissions.any { it.uri == uri && it.isWritePermission }) {
          return uri
        }
        mirrorPrefs.edit().remove(path).apply()
      }
    }
    return sessionMirror[path]?.let(Uri::parse)
  }

  /// Called with the URI of an intent-opened document: keep write access to
  /// the original automatically when the grant (or a permission) allows it.
  private fun autoLink(uri: Uri, path: String) {
    if (mirrorPrefs.getString(path, null) != null) return // manual link wins
    try {
      contentResolver.takePersistableUriPermission(
        uri,
        Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
      )
      // commit(), NOT apply(): on a warm open the process exits right after
      // staging (see onNewIntent), killing apply()'s async disk write.
      mirrorPrefs.edit().putString(path, uri.toString()).commit()
      return
    } catch (e: Exception) { /* sender didn't offer a persistable grant */ }
    // Optional "All files access": if the user granted it, resolve the URI to
    // its raw path and link durably — works even for read-only "open with"
    // grants, and for USB/SD volumes.
    if (hasAllFilesAccess()) {
      rawPathFor(uri)?.let { f ->
        mirrorPrefs.edit().putString(path, Uri.fromFile(f).toString()).commit()
        return
      }
    }
    // Transient grant: usable only while this process lives, and only if it
    // includes write. checkUriPermission is a non-destructive probe (opening
    // the stream to test would truncate the file).
    val canWrite = checkUriPermission(
      uri, android.os.Process.myPid(), android.os.Process.myUid(),
      Intent.FLAG_GRANT_WRITE_URI_PERMISSION
    ) == android.content.pm.PackageManager.PERMISSION_GRANTED
    if (canWrite) sessionMirror[path] = uri.toString()
  }

  /// MANAGE_EXTERNAL_STORAGE granted? (The optional "All files access"
  /// toggle in system Settings; API 30+.)
  private fun hasAllFilesAccess(): Boolean =
    android.os.Build.VERSION.SDK_INT >= 30 && android.os.Environment.isExternalStorageManager()

  /// Resolve a document URI to its raw shared-storage path, when the URI
  /// exposes one (the system externalstorage provider embeds
  /// "volume:relative/path" in the document id). Only meaningful with
  /// all-files access — without it the path isn't writable anyway.
  private fun rawPathFor(uri: Uri): File? = try {
    when {
      uri.scheme == "file" -> uri.path?.let(::File)
      uri.authority == "com.android.externalstorage.documents" -> {
        val doc = android.provider.DocumentsContract.getDocumentId(uri)
        val i = doc.indexOf(':')
        if (i < 0) null else {
          val vol = doc.substring(0, i)
          val base = if (vol == "primary") {
            @Suppress("DEPRECATION") android.os.Environment.getExternalStorageDirectory()
          } else File("/storage/$vol")
          File(base, doc.substring(i + 1))
        }
      }
      else -> null
    }?.takeIf { it.isFile }
  } catch (e: Exception) { null }

  /// Ask the shell to mirror now (it fetches a consistent export and hands
  /// the bytes to writeLinked). Must run on the UI thread.
  private fun flushMirror() {
    findWebView(findViewById(android.R.id.content))
      ?.evaluateJavascript("window.__uappMirrorFlush && window.__uappMirrorFlush();", null)
  }

  override fun onPause() {
    super.onPause()
    // The user is leaving — make sure the linked original is current.
    flushMirror()
  }

  // Diagnosing a blanked-out page has to start from what is actually running:
  // WebGL behaviour — including how many live contexts a renderer will hold
  // before it starts dropping them — varies widely between WebView builds, and
  // a renderer that has merely gone unresponsive looks identical from the
  // outside to one that died. Record both.
  //
  // A renderer that DIES cannot be recovered from here: onRenderProcessGone is
  // a WebViewClient method, and wry installs its own client (RustWebViewClient,
  // generated into this project at build time) which we cannot replace without
  // reimplementing its request interception. With no such override the
  // framework kills the process outright — so a white screen with the app still
  // running is evidence AGAINST a dead renderer, and for something the page
  // itself lost, like a GL context.
  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    val pkg = WebViewCompat.getCurrentWebViewPackage(this)
    Log.i(TAG, "webview provider: ${pkg?.packageName} ${pkg?.versionName}")
    if (!WebViewFeature.isFeatureSupported(WebViewFeature.WEB_VIEW_RENDERER_CLIENT_BASIC_USAGE)) {
      Log.i(TAG, "renderer client unsupported on this WebView — no liveness logging")
      return
    }
    WebViewCompat.setWebViewRenderProcessClient(webView, object : WebViewRenderProcessClient() {
      override fun onRenderProcessUnresponsive(view: WebView, renderer: WebViewRenderProcess?) {
        Log.w(TAG, "renderer unresponsive — the page has stopped painting")
      }

      override fun onRenderProcessResponsive(view: WebView, renderer: WebViewRenderProcess?) {
        Log.i(TAG, "renderer responsive again")
      }
    })
  }

  // Memory pressure precedes both a renderer kill and the WebView throwing away
  // GPU resources, so record it: a canvas that blanks right after a
  // TRIM_MEMORY_RUNNING_CRITICAL line is a different bug from one that blanks
  // with no pressure reported at all.
  override fun onTrimMemory(level: Int) {
    super.onTrimMemory(level)
    Log.i(TAG, "onTrimMemory level=$level")
  }

  private fun findWebView(view: View): WebView? = when (view) {
    is WebView -> view
    is ViewGroup -> {
      var found: WebView? = null
      for (i in 0 until view.childCount) {
        found = findWebView(view.getChildAt(i)) ?: found
        if (found != null) break
      }
      found
    }
    else -> null
  }

  // Fallback download path: the shell's current downloads fetch in-page and
  // go through the saveBlob bridge below, but plain link navigations the
  // WebView can't render (and older shells' <a download> clicks) still land
  // here. DownloadManager fetches OUTSIDE the WebView (no cookies of its own
  // — forward them), and without an explicit public destination it writes to
  // an invisible system cache, so the file must be pointed at the user's
  // Downloads folder to be findable at all.
  private fun enqueueDownload(url: String, contentDisposition: String?, mimeType: String?) {
    try {
      val name = android.webkit.URLUtil.guessFileName(url, contentDisposition, mimeType)
      val req = DownloadManager.Request(Uri.parse(url))
        .setNotificationVisibility(DownloadManager.Request.VISIBILITY_VISIBLE_NOTIFY_COMPLETED)
        .setDestinationInExternalPublicDir(android.os.Environment.DIRECTORY_DOWNLOADS, name)
        .setTitle(name)
        .setAllowedOverMetered(true)
        .setAllowedOverRoaming(true)
      CookieManager.getInstance().getCookie(url)?.let { req.addRequestHeader("Cookie", it) }
      if (!mimeType.isNullOrEmpty()) req.setMimeType(mimeType)
      getSystemService(DownloadManager::class.java).enqueue(req)
    } catch (e: Exception) {
      Toast.makeText(this, "Download failed: ${e.message}", Toast.LENGTH_LONG).show()
    }
  }

  // Copy the intent's .uapp payload into the app's private dir under marker
  // names the Rust core checks at startup (see resolve_app_path in gui.rs).
  // Returns true if a document was staged.
  private fun handleViewIntent(intent: Intent?): Boolean {
    if (intent?.action != Intent.ACTION_VIEW || intent.data == null) return false
    val uri: Uri = intent.data ?: return false
    return try {
      // Prefer the provider's display name: many providers (system Downloads
      // among them) use opaque numeric document ids in the URI path, so
      // parsing lastPathSegment would name the import "1234.uapp".
      val raw = queryDisplayName(uri)
        ?: uri.lastPathSegment?.substringAfterLast(':')?.substringAfterLast('/') ?: ""
      // Sanitize exactly like the Rust side (resolve_app_path in gui.rs) so
      // the imported working path is predictable HERE — autoLink below keys
      // the write-back mapping on it.
      var name = raw.trim()
        .map { if (it == '/' || it == '\\' || it.isISOControl()) '-' else it }
        .joinToString("").trim('.').trim()
      if (name.isEmpty()) name = "Shared App"
      if (!name.endsWith(".uapp")) name = "$name.uapp"
      contentResolver.openInputStream(uri)!!.use { input ->
        File(markerDir, PENDING_DOC).outputStream().use { input.copyTo(it) }
      }
      File(markerDir, PENDING_NAME).writeText(name)
      // Keep write access to the tapped original when the grant allows it,
      // so changes mirror back without the manual Settings → Link step.
      val dest = File(File(markerDir, "UApp"), name).absolutePath
      // Remembered for retryAutoLink after a permission grant. Persisted, not
      // in-memory: a warm open restarts the process (see onNewIntent), which
      // would otherwise lose the source before Settings can use it. Storing
      // the URI grants nothing by itself — using it still requires access.
      // commit(), NOT apply(): the warm path calls exit(0) right after this
      // returns, which would kill apply()'s async disk write.
      sourcePrefs.edit().putString(dest, uri.toString()).commit()
      autoLink(uri, dest)
      true
    } catch (e: Exception) {
      Toast.makeText(this, "Could not open document: ${e.message}", Toast.LENGTH_LONG).show()
      false
    }
  }

  // ---- install to launcher (home-screen shortcut for one .uapp) ----

  // JS bridge for the shell's Install button: pins a launcher shortcut that
  // opens this exact .uapp. Runs on a JS bridge thread; touches only
  // thread-safe APIs (ShortcutManagerCompat + Intent).
  inner class ShellBridge {
    @JavascriptInterface
    fun addShortcut(name: String, path: String) = addShortcutIcon(name, path, "")

    // Same, with an optional custom icon (base64 PNG from the shell's picker).
    // Separate name rather than an overload: the WebView bridge resolves
    // methods by name and older shells still call the two-arg form.
    @JavascriptInterface
    fun addShortcutIcon(name: String, path: String, iconB64: String) {
      try {
        val launch = Intent(this@MainActivity, MainActivity::class.java).apply {
          action = Intent.ACTION_MAIN
          flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK
          putExtra(EXTRA_SHORTCUT_PATH, path)
        }
        // Adaptive so launchers mask it like any app icon; a bad/undecodable
        // image quietly falls back to the app's own icon.
        val icon = if (iconB64.isNotEmpty()) {
          try {
            val bytes = android.util.Base64.decode(iconB64, android.util.Base64.DEFAULT)
            android.graphics.BitmapFactory.decodeByteArray(bytes, 0, bytes.size)
              ?.let { IconCompat.createWithAdaptiveBitmap(it) }
          } catch (e: Exception) { null }
        } else null
        val info = ShortcutInfoCompat.Builder(this@MainActivity, "uapp:" + File(path).name)
          .setShortLabel(name.take(12))
          .setLongLabel(name)
          .setIcon(icon ?: IconCompat.createWithResource(this@MainActivity, applicationInfo.icon))
          .setIntent(launch)
          .build()
        // requestPinShortcut shows the system confirm dialog; no permission
        // needed. On launchers without pinning support, fall back to a toast.
        val ok = ShortcutManagerCompat.requestPinShortcut(this@MainActivity, info, null)
        if (!ok) runOnUiThread {
          Toast.makeText(this@MainActivity,
            "This launcher doesn't support pinned shortcuts", Toast.LENGTH_LONG).show()
        }
      } catch (e: Exception) {
        runOnUiThread {
          Toast.makeText(this@MainActivity, "Install failed: ${e.message}", Toast.LENGTH_LONG).show()
        }
      }
    }

    // ---- original-file write-back (see the mirror helpers on the activity) ----

    @JavascriptInterface
    fun linkOriginal(path: String) {
      pendingLinkPath = path
      runOnUiThread {
        try {
          pickOriginal.launch(arrayOf("*/*"))
        } catch (e: Exception) {
          Toast.makeText(this@MainActivity, "Could not open the file picker: ${e.message}", Toast.LENGTH_LONG).show()
        }
      }
    }

    @JavascriptInterface
    fun unlinkOriginal(path: String) {
      linkedUri(path)?.let { uri ->
        try {
          contentResolver.releasePersistableUriPermission(
            uri,
            Intent.FLAG_GRANT_READ_URI_PERMISSION or Intent.FLAG_GRANT_WRITE_URI_PERMISSION
          )
        } catch (e: Exception) { /* grant already gone */ }
      }
      mirrorPrefs.edit().remove(path).apply()
      sessionMirror.remove(path)
    }

    @JavascriptInterface
    fun mirrorStatus(path: String): String =
      if (linkedUri(path) != null) "linked" else "none"

    // ---- optional "All files access" (see autoLink's raw-path tier) ----

    @JavascriptInterface
    fun allFilesAccess(): String = when {
      android.os.Build.VERSION.SDK_INT < 30 -> "unsupported"
      hasAllFilesAccess() -> "granted"
      else -> "denied"
    }

    @JavascriptInterface
    fun requestAllFilesAccess() {
      if (android.os.Build.VERSION.SDK_INT < 30) return
      runOnUiThread {
        try {
          startActivity(Intent(
            android.provider.Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
            Uri.parse("package:$packageName")))
        } catch (e: Exception) {
          // Some OEM skins lack the per-app screen; fall back to the list.
          try {
            startActivity(Intent(android.provider.Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION))
          } catch (e2: Exception) {
            Toast.makeText(this@MainActivity, "Could not open the settings screen", Toast.LENGTH_LONG).show()
          }
        }
      }
    }

    /// After the user grants all-files access mid-session, re-run autoLink
    /// with the intent URI the current document was opened from, so it links
    /// without requiring a reopen.
    @JavascriptInterface
    fun retryAutoLink(path: String) {
      sourcePrefs.getString(path, null)?.let { autoLink(Uri.parse(it), path) }
    }

    @JavascriptInterface
    fun writeLinked(path: String, b64: String) {
      val uri = linkedUri(path) ?: return
      try {
        val bytes = android.util.Base64.decode(b64, android.util.Base64.DEFAULT)
        if (uri.scheme == "file") {
          // Raw path via all-files access: write a sibling temp file and
          // rename over the original, so a mid-write crash can't leave a
          // truncated document.
          val f = File(uri.path!!)
          val tmp = File(f.parentFile, f.name + ".uapp-tmp")
          tmp.writeBytes(bytes)
          if (!tmp.renameTo(f)) { f.writeBytes(bytes); tmp.delete() }
          return
        }
        // "wt" truncates; some providers only accept "rwt" (and plain "w"
        // famously may NOT truncate, leaving trailing garbage — never use it).
        val out = try { contentResolver.openOutputStream(uri, "wt") } catch (e: Exception) { null }
          ?: contentResolver.openOutputStream(uri, "rwt")
          ?: throw Exception("provider refused to open the file")
        out.use { it.write(bytes) }
      } catch (e: SecurityException) {
        mirrorPrefs.edit().remove(path).apply()
        sessionMirror.remove(path)
        runOnUiThread {
          Toast.makeText(this@MainActivity, "Lost access to the original file — relink it in Settings", Toast.LENGTH_LONG).show()
        }
      } catch (e: Exception) {
        runOnUiThread {
          Toast.makeText(this@MainActivity, "Write-back failed: ${e.message}", Toast.LENGTH_LONG).show()
        }
      }
    }

    // The shell's client-generated exports (CSV/TSV/XLSX) are blob: URLs,
    // which DownloadManager cannot fetch — the shell hands us the bytes
    // instead and we write them straight into the system Downloads folder.
    @JavascriptInterface
    fun saveBlob(name: String, mime: String, b64: String) {
      try {
        val bytes = android.util.Base64.decode(b64, android.util.Base64.DEFAULT)
        val safe = name.replace(Regex("[/\\\\\u0000]"), "_").ifBlank { "download" }
        if (android.os.Build.VERSION.SDK_INT >= 29) {
          val values = android.content.ContentValues().apply {
            put(android.provider.MediaStore.Downloads.DISPLAY_NAME, safe)
            put(android.provider.MediaStore.Downloads.MIME_TYPE, mime.ifBlank { "application/octet-stream" })
            put(android.provider.MediaStore.Downloads.IS_PENDING, 1)
          }
          val uri = contentResolver.insert(
            android.provider.MediaStore.Downloads.EXTERNAL_CONTENT_URI, values
          ) ?: throw Exception("could not create download entry")
          contentResolver.openOutputStream(uri)!!.use { it.write(bytes) }
          values.clear()
          values.put(android.provider.MediaStore.Downloads.IS_PENDING, 0)
          contentResolver.update(uri, values, null, null)
        } else {
          // Pre-Q: public Downloads is directly writable with no permission
          // prompt only via the legacy path; fall back to app-external files.
          val dir = getExternalFilesDir(android.os.Environment.DIRECTORY_DOWNLOADS)
            ?: throw Exception("no storage available")
          File(dir, safe).writeBytes(bytes)
        }
        runOnUiThread {
          Toast.makeText(this@MainActivity, "Saved to Downloads: $safe", Toast.LENGTH_LONG).show()
        }
      } catch (e: Exception) {
        runOnUiThread {
          Toast.makeText(this@MainActivity, "Save failed: ${e.message}", Toast.LENGTH_LONG).show()
        }
      }
    }
  }

  // Launched from a pinned shortcut: write the target .uapp path to a marker
  // the Rust core checks at startup (see resolve_app_path in gui.rs). Returns
  // true if a shortcut was staged (caller may restart the process).
  private fun handleShortcutIntent(intent: Intent?): Boolean {
    val path = intent?.getStringExtra(EXTRA_SHORTCUT_PATH) ?: return false
    if (!File(path).isAbsolute || !path.endsWith(".uapp")) return false
    return try {
      File(markerDir, SHORTCUT_PATH).writeText(path)
      true
    } catch (e: Exception) {
      Toast.makeText(this, "Could not open shortcut: ${e.message}", Toast.LENGTH_LONG).show()
      false
    }
  }

  // Where the Rust core looks for the marker files: Tauri's app_data_dir()
  // on Android is the package data ROOT (Context.dataDir), NOT filesDir —
  // markers staged into filesDir are silently never consumed, so "open with"
  // opens the default document instead (verified on-device).
  private val markerDir: File get() = dataDir

  companion object {
    // Transient auto-links (write grant valid only while this process lives);
    // process-static so an activity recreation doesn't lose them.
    private val sessionMirror = HashMap<String, String>()
    private const val PENDING_DOC = "pending_open.uapp"
    private const val PENDING_NAME = "pending_open.name"
    private const val EXTRA_SHORTCUT_PATH = "uapp.shortcut_path"
    private const val SHORTCUT_PATH = "shortcut_open.path"
    private val CHROME_COLOR = Color.parseColor("#23293A")
    // One tag for everything worth grepping out of logcat: `adb logcat -s uapp`.
    private const val TAG = "uapp"
  }
}
