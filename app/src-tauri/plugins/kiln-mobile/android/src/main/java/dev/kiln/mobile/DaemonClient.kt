package dev.kiln.mobile

import android.content.Context
import org.json.JSONObject
import java.io.OutputStreamWriter
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.Executors

/**
 * Minimal HTTP client for the two calls the phone makes without the
 * WebView: registering the push endpoint and acting on a notification
 * (`POST /api/v1/notifications/{id}/action`). Bearer auth with the stored
 * session token; the daemon is reached over the tailnet like the app.
 */
object DaemonClient {
    private val io = Executors.newSingleThreadExecutor()

    data class Result(val ok: Boolean, val status: Int, val body: JSONObject?, val error: String?)

    private fun post(ctx: Context, path: String, json: JSONObject): Result {
        val server = KilnStore.server(ctx) ?: return Result(false, 0, null, "Not signed in")
        val token = KilnStore.token(ctx) ?: return Result(false, 0, null, "Not signed in")
        var conn: HttpURLConnection? = null
        return try {
            conn = (URL("$server/api/v1$path").openConnection() as HttpURLConnection).apply {
                requestMethod = "POST"
                connectTimeout = 8000
                readTimeout = 15000
                doOutput = true
                setRequestProperty("Content-Type", "application/json")
                setRequestProperty("Accept", "application/json")
                setRequestProperty("Authorization", "Bearer $token")
            }
            OutputStreamWriter(conn.outputStream, Charsets.UTF_8).use { it.write(json.toString()) }
            val code = conn.responseCode
            val stream = if (code in 200..299) conn.inputStream else conn.errorStream
            val text = stream?.bufferedReader(Charsets.UTF_8)?.use { it.readText() } ?: ""
            val body = try { if (text.isBlank()) null else JSONObject(text) } catch (_: Exception) { null }
            if (code in 200..299) {
                Result(true, code, body, null)
            } else {
                Result(false, code, body, body?.optString("error")?.takeIf { it.isNotBlank() } ?: "HTTP $code")
            }
        } catch (e: Exception) {
            Result(false, 0, null, e.message ?: "network error")
        } finally {
            conn?.disconnect()
        }
    }

    /** `POST /devices/push` (RegisterPushRequest). Runs off the main thread. */
    fun registerPush(ctx: Context, endpoint: String, done: ((Result) -> Unit)? = null) {
        val app = ctx.applicationContext
        io.execute {
            val r = post(app, "/devices/push", JSONObject()
                .put("endpoint", endpoint)
                .put("device_name", KilnStore.deviceName(app)))
            done?.invoke(r)
        }
    }

    /** `POST /notifications/{id}/action` (NotificationActionRequest). */
    fun notificationAction(ctx: Context, id: String, action: String, text: String?, done: (Result) -> Unit) {
        val app = ctx.applicationContext
        io.execute {
            val body = JSONObject().put("action", action)
            if (text != null) body.put("text", text)
            done(post(app, "/notifications/$id/action", body))
        }
    }
}
