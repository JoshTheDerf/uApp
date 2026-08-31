package com.thederf.uapp

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log
import java.net.HttpURLConnection
import java.net.URL

/**
 * Keeps an in-flight AI chat run alive while the app is in the background.
 *
 * Without this, Android puts the whole process into the cached-apps freezer
 * shortly after the user switches away: the Rust server thread stops mid-turn
 * and its provider socket is dead by the time the process thaws, so every
 * backgrounded chat ended in a connection error. A dataSync foreground
 * service exempts the process from the freezer, so the turn keeps running
 * and the finished reply is waiting when the user comes back.
 *
 * Started/stopped by the shell (ShellBridge.chatKeepalive) when a run begins
 * and ends. The shell's "run ended" call can itself be frozen with the page,
 * so the service also watches the core directly: it polls the local server's
 * /health (x-uapp-ai-busy) and stops itself once no run is live — and after a
 * hard cap, so a wedged run can never pin the process forever (dataSync time
 * is budgeted per day on Android 14+).
 */
class ChatWorkService : Service() {
  private var watcher: Thread? = null

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    startInForeground()
    val healthUrl = intent?.getStringExtra(EXTRA_HEALTH_URL)
    if (watcher == null) {
      watcher = Thread { watch(healthUrl) }.apply { isDaemon = true; start() }
    }
    return START_NOT_STICKY
  }

  override fun onDestroy() {
    watcher?.interrupt()
    watcher = null
    super.onDestroy()
  }

  private fun startInForeground() {
    val nm = getSystemService(NotificationManager::class.java)
    if (Build.VERSION.SDK_INT >= 26) {
      nm.createNotificationChannel(
        NotificationChannel(
          CHANNEL, getString(R.string.chat_work_channel), NotificationManager.IMPORTANCE_LOW
        ).apply { setShowBadge(false) }
      )
    }
    val open = PendingIntent.getActivity(
      this, 0, Intent(this, MainActivity::class.java),
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
    )
    val notif: Notification = (
      if (Build.VERSION.SDK_INT >= 26) Notification.Builder(this, CHANNEL)
      else @Suppress("DEPRECATION") Notification.Builder(this).setPriority(Notification.PRIORITY_LOW)
    )
      .setSmallIcon(android.R.drawable.stat_notify_sync)
      .setContentTitle(getString(R.string.chat_work_running))
      .setContentIntent(open)
      .setOngoing(true)
      .build()
    if (Build.VERSION.SDK_INT >= 29) {
      startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
    } else {
      startForeground(NOTIF_ID, notif)
    }
  }

  /// Poll the core until no AI run is live, then stop. Two consecutive idle
  /// (or unreachable — the server lives in this same process, so unreachable
  /// means it is gone) reads end the service; a hard cap bounds the worst case.
  private fun watch(healthUrl: String?) {
    val deadline = System.currentTimeMillis() + MAX_MS
    var idleReads = 0
    try {
      while (System.currentTimeMillis() < deadline) {
        Thread.sleep(POLL_MS)
        // No URL (shouldn't happen): rely on the shell's stop + the hard cap.
        if (healthUrl == null) continue
        if (aiBusy(healthUrl) == true) idleReads = 0 else idleReads++
        if (idleReads >= 2) break
      }
    } catch (e: InterruptedException) {
      return // onDestroy — already stopping
    }
    Log.i(TAG, "chat keep-alive done")
    stopSelf()
  }

  /// One /health probe: true/false from x-uapp-ai-busy, null when unreachable.
  private fun aiBusy(url: String): Boolean? = try {
    val c = URL(url).openConnection() as HttpURLConnection
    c.connectTimeout = 3000
    c.readTimeout = 3000
    try {
      if (c.responseCode != 200) null
      else c.getHeaderField("x-uapp-ai-busy") == "1"
    } finally {
      c.disconnect()
    }
  } catch (e: Exception) { null }

  companion object {
    const val EXTRA_HEALTH_URL = "uapp.health_url"
    private const val CHANNEL = "uapp_chat_work"
    private const val NOTIF_ID = 1001
    private const val POLL_MS = 5_000L
    private const val MAX_MS = 2 * 60 * 60 * 1000L // dataSync time is budgeted; cap a run at 2h
    private const val TAG = "uapp"
  }
}
