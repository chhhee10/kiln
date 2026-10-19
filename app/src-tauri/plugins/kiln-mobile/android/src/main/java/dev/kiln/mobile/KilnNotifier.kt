package dev.kiln.mobile

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Typeface
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import org.json.JSONObject

/**
 * Native notifications for daemon pushes (design/docs/android.md
 * "Notifications"). Payload (crates/notifier `PushPayload`):
 * `{ id, kind, channel, title, body, deep_link, actions:[{id,label}], progress?, resolved? }`.
 *
 * Channels: Decisions (high, heads-up) · Failures (high) · Ready for review
 * (default) · Live workers (ongoing, silent, progress bar).
 * Inline actions (retry, approve, keep shared, stop, escalate) POST to the
 * daemon from [NotificationActionReceiver] without opening the app;
 * navigation actions (open, review, view, decide, reply) and taps open the
 * app on the matching deep link.
 */
object KilnNotifier {
    const val EXTRA_LINK = "dev.kiln.mobile.LINK"
    const val EXTRA_ID = "dev.kiln.mobile.ID"
    const val EXTRA_ACTION = "dev.kiln.mobile.ACTION"
    const val EXTRA_TITLE = "dev.kiln.mobile.TITLE"
    const val EXTRA_CHANNEL = "dev.kiln.mobile.CHANNEL"
    const val ACTION_NOTIFICATION = "dev.kiln.mobile.NOTIFICATION_ACTION"
    private const val GROUP = "dev.kiln.mobile.GROUP"
    private const val SUMMARY_ID = 0x4b494c4e // "KILN"

    private const val LIME = 0xFFCDF04A.toInt()

    /** Actions that only navigate (the daemon just marks the card read). */
    private val NAV_ACTIONS = setOf("open", "review", "view", "decide", "reply")

    data class Channel(val id: String, val name: String, val desc: String, val importance: Int)

    val CHANNELS = listOf(
        Channel("decisions", "Decisions", "Plans and memories waiting on you", NotificationManager.IMPORTANCE_HIGH),
        Channel("failures", "Failures", "QA or reviewer failures", NotificationManager.IMPORTANCE_HIGH),
        Channel("ready_for_review", "Ready for review", "PRs ready for human review", NotificationManager.IMPORTANCE_DEFAULT),
        Channel("live_workers", "Live workers", "Ongoing worker progress", NotificationManager.IMPORTANCE_LOW),
    )

    fun ensureChannels(ctx: Context) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val nm = ctx.getSystemService(NotificationManager::class.java) ?: return
        for (c in CHANNELS) {
            val ch = NotificationChannel(c.id, c.name, c.importance).apply {
                description = c.desc
                if (c.id == "live_workers") {
                    setSound(null, null)
                    enableVibration(false)
                    setShowBadge(false)
                }
            }
            nm.createNotificationChannel(ch)
        }
    }

    /** Stable Android notification id for a daemon notification UUID. */
    fun notificationId(id: String): Int = id.hashCode().let { if (it == SUMMARY_ID) it + 1 else it }

    /** Glyph + status color per kind (design table). */
    private fun glyph(kind: String): Pair<String, Int> = when (kind) {
        "plan_ready" -> "◇" to LIME
        "task_failed" -> "✕" to 0xFFF0605A.toInt()
        "task_escalated" -> "!" to 0xFFF2C14E.toInt()
        "task_ready" -> "✓" to 0xFF4FD19A.toInt()
        "memory_approval" -> "◑" to 0xFFDDE6C8.toInt()
        "workers_live" -> "◐" to 0xFF5FA8F0.toInt()
        else -> "!" to 0xFFF0605A.toInt()
    }

    /** 36dp circle, glyph in the status color on a dark tint of it. */
    private fun largeIcon(ctx: Context, kind: String): Bitmap {
        val (g, color) = glyph(kind)
        val px = (36 * ctx.resources.displayMetrics.density).toInt().coerceAtLeast(36)
        val bmp = Bitmap.createBitmap(px, px, Bitmap.Config.ARGB_8888)
        val c = Canvas(bmp)
        val bg = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            // Status color at ~25% over the M3 surface (#11130e).
            this.color = blend(0xFF11130E.toInt(), color, 0.25f)
        }
        c.drawCircle(px / 2f, px / 2f, px / 2f, bg)
        val fg = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            this.color = color
            textSize = px * 0.45f
            textAlign = Paint.Align.CENTER
            typeface = Typeface.MONOSPACE
        }
        val y = px / 2f - (fg.descent() + fg.ascent()) / 2f
        c.drawText(g, px / 2f, y, fg)
        return bmp
    }

    private fun blend(a: Int, b: Int, t: Float): Int {
        fun ch(x: Int, s: Int) = (x shr s) and 0xff
        fun mix(s: Int) = (ch(a, s) * (1 - t) + ch(b, s) * t).toInt()
        return Color.argb(255, mix(16), mix(8), mix(0))
    }

    /** Deep link for a tap or a navigation action (see android/nav.rs). */
    fun linkFor(p: JSONObject, action: String?): String {
        val link = p.optString("deep_link").ifBlank { "chat" }
        val kind = p.optString("kind")
        return when {
            kind == "workers_live" -> "runs"
            action == "reply" || action == "decide" -> if (link.startsWith("task/")) "$link/feedback" else link
            action == "review" && link.startsWith("plan/") -> "$link/review"
            action == "view" && kind == "task_ready" -> "chat/prs"
            kind == "task_escalated" && action == null && link.startsWith("task/") -> "$link/feedback"
            else -> link
        }
    }

    /** Intent that opens (or brings back) the app on `link`. */
    fun openAppIntent(ctx: Context, link: String, requestCode: Int): PendingIntent? {
        val launch = ctx.packageManager.getLaunchIntentForPackage(ctx.packageName) ?: return null
        launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP or Intent.FLAG_ACTIVITY_CLEAR_TOP)
        launch.putExtra(EXTRA_LINK, link)
        return PendingIntent.getActivity(
            ctx, requestCode, launch,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
    }

    private fun actionIntent(ctx: Context, id: String, action: String, title: String, channel: String, requestCode: Int): PendingIntent {
        val i = Intent(ctx, NotificationActionReceiver::class.java).apply {
            this.action = ACTION_NOTIFICATION
            putExtra(EXTRA_ID, id)
            putExtra(EXTRA_ACTION, action)
            putExtra(EXTRA_TITLE, title)
            putExtra(EXTRA_CHANNEL, channel)
        }
        return PendingIntent.getBroadcast(
            ctx, requestCode, i,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
    }

    private fun canNotify(ctx: Context): Boolean = NotificationManagerCompat.from(ctx).areNotificationsEnabled()

    /** Show / update / resolve a notification from a push payload. */
    fun show(ctx: Context, p: JSONObject) {
        if (!canNotify(ctx)) return
        ensureChannels(ctx)
        val id = p.optString("id").ifBlank { return }
        val nid = notificationId(id)
        val kind = p.optString("kind")
        val channel = p.optString("channel").ifBlank { "decisions" }
        val title = p.optString("title")
        val body = p.optString("body")
        val resolved = if (p.isNull("resolved")) null else p.optString("resolved").takeIf { it.isNotBlank() }
        val nm = NotificationManagerCompat.from(ctx)

        if (resolved != null) {
            if (channel == "live_workers") {
                nm.cancel(nid)
            } else {
                showResolved(ctx, id, title, channel, resolved)
            }
            return
        }

        val b = NotificationCompat.Builder(ctx, channel)
            .setSmallIcon(R.drawable.ic_stat_kiln)
            .setColor(LIME)
            .setLargeIcon(largeIcon(ctx, kind))
            .setContentTitle(title)
            .setContentText(body)
            .setStyle(NotificationCompat.BigTextStyle().bigText(body))
            .setGroup(GROUP)
            .setContentIntent(openAppIntent(ctx, linkFor(p, null), nid))
            .setAutoCancel(true)
            .setCategory(
                when (channel) {
                    "live_workers" -> NotificationCompat.CATEGORY_PROGRESS
                    "failures" -> NotificationCompat.CATEGORY_ERROR
                    else -> NotificationCompat.CATEGORY_EVENT
                }
            )
            .setPriority(
                when (channel) {
                    "decisions", "failures" -> NotificationCompat.PRIORITY_HIGH
                    "live_workers" -> NotificationCompat.PRIORITY_LOW
                    else -> NotificationCompat.PRIORITY_DEFAULT
                }
            )

        if (channel == "live_workers") {
            val progress = if (p.isNull("progress")) -1 else p.optInt("progress", -1)
            b.setOngoing(true)
                .setOnlyAlertOnce(true)
                .setSilent(true)
                .setAutoCancel(false)
                .setProgress(100, progress.coerceIn(0, 100), progress < 0)
        }

        val actions = p.optJSONArray("actions")
        if (actions != null) {
            for (i in 0 until minOf(actions.length(), 3)) {
                val a = actions.optJSONObject(i) ?: continue
                val aid = a.optString("id").lowercase()
                val label = a.optString("label").ifBlank { aid }
                val pi = if (aid in NAV_ACTIONS) {
                    openAppIntent(ctx, linkFor(p, aid), nid * 31 + i)
                } else {
                    actionIntent(ctx, id, aid, title, channel, nid * 31 + i)
                }
                if (pi != null) b.addAction(0, label, pi)
            }
        }

        notifySafely(ctx, nid, b)
        if (channel != "live_workers") summary(ctx)
    }

    /** "✓ Retried · attempt 3/3 running", then dismiss after a moment. */
    fun showResolved(ctx: Context, id: String, title: String, channel: String, text: String) {
        val nid = notificationId(id)
        val line = if (text.startsWith("✓")) text else "✓ $text"
        val b = NotificationCompat.Builder(ctx, channel)
            .setSmallIcon(R.drawable.ic_stat_kiln)
            .setColor(LIME)
            .setContentTitle(title)
            .setContentText(line)
            .setGroup(GROUP)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            .setAutoCancel(true)
            .setTimeoutAfter(4000)
        notifySafely(ctx, nid, b)
    }

    /** The action failed: say so and offer to open the app instead. */
    fun showActionError(ctx: Context, id: String, title: String, channel: String, error: String) {
        val nid = notificationId(id)
        val b = NotificationCompat.Builder(ctx, channel)
            .setSmallIcon(R.drawable.ic_stat_kiln)
            .setColor(LIME)
            .setContentTitle(title)
            .setContentText("Couldn't do that: $error · tap to open Kiln")
            .setGroup(GROUP)
            .setOnlyAlertOnce(true)
            .setAutoCancel(true)
            .setContentIntent(openAppIntent(ctx, "chat", nid))
        notifySafely(ctx, nid, b)
    }

    /** Group summary so several cards stack with M3 grouped corners. */
    private fun summary(ctx: Context) {
        val b = NotificationCompat.Builder(ctx, "decisions")
            .setSmallIcon(R.drawable.ic_stat_kiln)
            .setColor(LIME)
            .setGroup(GROUP)
            .setGroupSummary(true)
            .setGroupAlertBehavior(NotificationCompat.GROUP_ALERT_CHILDREN)
            .setSilent(true)
            .setAutoCancel(true)
        notifySafely(ctx, SUMMARY_ID, b)
    }

    private fun notifySafely(ctx: Context, nid: Int, b: NotificationCompat.Builder) {
        try {
            NotificationManagerCompat.from(ctx).notify(nid, b.build())
        } catch (_: SecurityException) {
            // POST_NOTIFICATIONS was revoked; nothing to do.
        }
    }
}
