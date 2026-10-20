package dev.kiln.mobile

import android.content.Context
import android.content.SharedPreferences

/**
 * What the background parts (push service, action receiver) need when the
 * WebView isn't running: the daemon URL, the session token and the current
 * UnifiedPush endpoint. App-private SharedPreferences (MODE_PRIVATE; the
 * token is the same bearer the WebView keeps in its own storage).
 */
object KilnStore {
    private const val FILE = "kiln_mobile"
    private const val SERVER = "server"
    private const val TOKEN = "token"
    private const val DEVICE = "device_name"
    private const val ENDPOINT = "endpoint"
    private const val P256DH = "p256dh"
    private const val AUTH = "auth"

    private fun prefs(ctx: Context): SharedPreferences =
        ctx.applicationContext.getSharedPreferences(FILE, Context.MODE_PRIVATE)

    fun server(ctx: Context): String? = prefs(ctx).getString(SERVER, null)?.takeIf { it.isNotBlank() }
    fun token(ctx: Context): String? = prefs(ctx).getString(TOKEN, null)?.takeIf { it.isNotBlank() }
    fun deviceName(ctx: Context): String = prefs(ctx).getString(DEVICE, null) ?: "Kiln Android"
    fun endpoint(ctx: Context): String? = prefs(ctx).getString(ENDPOINT, null)

    fun saveSession(ctx: Context, server: String, token: String, deviceName: String) {
        prefs(ctx).edit()
            .putString(SERVER, server.trimEnd('/'))
            .putString(TOKEN, token)
            .putString(DEVICE, deviceName)
            .apply()
    }

    /** Web Push keys `(p256dh, auth)` registered with the stored endpoint. */
    fun pushKeys(ctx: Context): Pair<String, String>? {
        val p = prefs(ctx)
        val pub = p.getString(P256DH, null) ?: return null
        val auth = p.getString(AUTH, null) ?: return null
        return pub to auth
    }

    /** Keys always travel with their endpoint: clearing one clears both. */
    fun saveEndpoint(ctx: Context, endpoint: String?, p256dh: String? = null, auth: String? = null) {
        val keep = endpoint != null && p256dh != null && auth != null
        prefs(ctx).edit()
            .putString(ENDPOINT, endpoint)
            .putString(P256DH, if (keep) p256dh else null)
            .putString(AUTH, if (keep) auth else null)
            .apply()
    }

    fun clear(ctx: Context) {
        prefs(ctx).edit().clear().apply()
    }
}
