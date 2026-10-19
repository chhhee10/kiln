package dev.kiln.mobile

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/**
 * Notification action buttons (Retry, Approve, Keep shared, Stop, …):
 * `POST /api/v1/notifications/{id}/action` with the stored session, without
 * opening the app. The daemon performs the command and resolves the card;
 * the notification is replaced by its "✓ …" line.
 */
class NotificationActionReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != KilnNotifier.ACTION_NOTIFICATION) return
        val id = intent.getStringExtra(KilnNotifier.EXTRA_ID) ?: return
        val action = intent.getStringExtra(KilnNotifier.EXTRA_ACTION) ?: return
        val title = intent.getStringExtra(KilnNotifier.EXTRA_TITLE) ?: "Kiln"
        val channel = intent.getStringExtra(KilnNotifier.EXTRA_CHANNEL) ?: "decisions"
        val app = context.applicationContext
        // Acknowledge the tap right away.
        KilnNotifier.showResolved(app, id, title, channel, "Working…")
        val pending = goAsync()
        DaemonClient.notificationAction(app, id, action, null) { r ->
            try {
                if (r.ok) {
                    val text = r.body?.optString("resolved")?.takeIf { it.isNotBlank() } ?: "Done"
                    KilnNotifier.showResolved(app, id, title, channel, text)
                } else {
                    KilnNotifier.showActionError(app, id, title, channel, r.error ?: "error")
                }
            } finally {
                pending.finish()
            }
        }
    }
}
