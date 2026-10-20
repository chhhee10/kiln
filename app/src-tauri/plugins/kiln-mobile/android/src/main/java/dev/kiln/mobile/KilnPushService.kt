package dev.kiln.mobile

import android.util.Log
import org.json.JSONObject
import org.unifiedpush.android.connector.FailedReason
import org.unifiedpush.android.connector.PushService
import org.unifiedpush.android.connector.data.PushEndpoint
import org.unifiedpush.android.connector.data.PushMessage

/**
 * UnifiedPush events from the distributor (e.g. ntfy).
 *
 * The connector generates a Web Push key pair per registration
 * (`endpoint.pubKeySet`); we hand the public half to the daemon, which then
 * encrypts every `PushPayload` to it (RFC 8291), so the distributor only
 * relays ciphertext. The connector decrypts before `onMessage`.
 */
class KilnPushService : PushService() {
    override fun onNewEndpoint(endpoint: PushEndpoint, instance: String) {
        val keys = endpoint.pubKeySet
        KilnStore.saveEndpoint(this, endpoint.url, keys?.pubKey, keys?.auth)
        val plugin = KilnMobilePlugin.instance
        if (plugin != null) {
            // The WebView registers it through the shared API layer.
            plugin.emitEndpoint(endpoint.url, keys?.pubKey, keys?.auth)
        } else {
            DaemonClient.registerPush(this, endpoint.url, keys?.pubKey, keys?.auth) { r ->
                if (!r.ok) Log.w(TAG, "push endpoint registration failed: ${r.error}")
            }
        }
    }

    override fun onMessage(message: PushMessage, instance: String) {
        if (!message.decrypted) {
            // Encrypted for an older key pair (or tampered with): drop it.
            Log.w(TAG, "ignoring a push the connector could not decrypt")
            return
        }
        val text = try {
            String(message.content, Charsets.UTF_8)
        } catch (_: Exception) {
            return
        }
        val payload = try {
            JSONObject(text)
        } catch (e: Exception) {
            Log.w(TAG, "ignoring non-JSON push (${text.length} bytes)")
            return
        }
        KilnNotifier.show(this, payload)
    }

    override fun onRegistrationFailed(reason: FailedReason, instance: String) {
        Log.w(TAG, "UnifiedPush registration failed: $reason")
        KilnMobilePlugin.instance?.emitPushFailed(
            when (reason) {
                FailedReason.NETWORK -> "No network · will retry"
                FailedReason.ACTION_REQUIRED -> "Open your UnifiedPush distributor (ntfy) to finish setup"
                FailedReason.VAPID_REQUIRED -> "The distributor requires VAPID, which Kiln doesn't use yet"
                else -> "Push registration failed"
            }
        )
    }

    override fun onUnregistered(instance: String) {
        KilnStore.saveEndpoint(this, null)
        KilnMobilePlugin.instance?.emitPushFailed("Push was unregistered by the distributor")
    }

    companion object {
        private const val TAG = "KilnPush"
    }
}
