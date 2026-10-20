package dev.kiln.mobile

import android.Manifest
import android.app.Activity
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.speech.RecognitionListener
import android.speech.RecognizerIntent
import android.speech.SpeechRecognizer
import android.webkit.WebView
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import android.content.pm.PackageManager
import app.tauri.PermissionState
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.Permission
import app.tauri.annotation.PermissionCallback
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import org.unifiedpush.android.connector.INSTANCE_DEFAULT
import org.unifiedpush.android.connector.UnifiedPush

@InvokeArg
class RegisterPushArgs {
    var server: String = ""
    var token: String = ""
    var deviceName: String = "Kiln Android"
}

@InvokeArg
class ListenArgs {
    var lang: String? = null
}

/**
 * WebView-facing side of the Kiln Android shell.
 *
 * Commands (invoked as `plugin:kiln-mobile|<snake_case>`):
 * - `register_push {server, token, deviceName}` → `{status, endpoint?}`;
 *   status is `registered` (endpoint known), `registering` (wait for the
 *   `pushEndpoint` event) or `no_distributor`.
 * - `unregister_push`
 * - `take_pending_link` → `{link?}` (a notification tap that launched the app)
 * - `start_listening {lang}` / `stop_listening` (Android SpeechRecognizer)
 *
 * Events (`addPluginListener("kiln-mobile", …)`): `pushEndpoint {endpoint}`,
 * `pushFailed {reason}`, `notificationTap {link}`,
 * `speech {type: level|partial|final|error|end, level?, text?, code?}`.
 */
@TauriPlugin(
    permissions = [
        Permission(strings = [Manifest.permission.RECORD_AUDIO], alias = "microphone"),
    ]
)
class KilnMobilePlugin(private val activity: Activity) : Plugin(activity) {
    private var recognizer: SpeechRecognizer? = null
    private var lastLevelAt = 0L

    companion object {
        @Volatile
        var instance: KilnMobilePlugin? = null

        @Volatile
        private var pendingLink: String? = null
    }

    override fun load(webView: WebView) {
        instance = this
        KilnNotifier.ensureChannels(activity)
        handleIntent(activity.intent)
    }

    override fun onNewIntent(intent: Intent) {
        handleIntent(intent)
    }

    override fun onDestroy(activity: androidx.appcompat.app.AppCompatActivity) {
        stopRecognizer()
        if (instance === this) instance = null
    }

    private fun handleIntent(intent: Intent?) {
        val link = intent?.getStringExtra(KilnNotifier.EXTRA_LINK) ?: return
        intent.removeExtra(KilnNotifier.EXTRA_LINK)
        pendingLink = link
        trigger("notificationTap", JSObject().put("link", link))
    }

    fun emitEndpoint(endpoint: String, p256dh: String?, auth: String?) {
        val o = JSObject().put("endpoint", endpoint)
        if (p256dh != null && auth != null) {
            o.put("p256dh", p256dh)
            o.put("auth", auth)
        }
        activity.runOnUiThread { trigger("pushEndpoint", o) }
    }

    fun emitPushFailed(reason: String) {
        activity.runOnUiThread { trigger("pushFailed", JSObject().put("reason", reason)) }
    }

    // ------------------------------------------------------------ links

    @Command
    fun takePendingLink(invoke: Invoke) {
        val o = JSObject()
        pendingLink?.let { o.put("link", it) }
        pendingLink = null
        invoke.resolve(o)
    }

    // ------------------------------------------------------------ push

    @Command
    fun registerPush(invoke: Invoke) {
        val args = invoke.parseArgs(RegisterPushArgs::class.java)
        if (args.server.isBlank() || args.token.isBlank()) {
            invoke.reject("server and token are required")
            return
        }
        KilnStore.saveSession(activity, args.server, args.token, args.deviceName)
        // Android 13+: notifications need a runtime grant.
        if (Build.VERSION.SDK_INT >= 33 &&
            ContextCompat.checkSelfPermission(activity, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            ActivityCompat.requestPermissions(activity, arrayOf(Manifest.permission.POST_NOTIFICATIONS), 0x4b4e)
        }
        val known = KilnStore.endpoint(activity)
        activity.runOnUiThread {
            UnifiedPush.tryUseCurrentOrDefaultDistributor(activity) { ok ->
                if (!ok) {
                    invoke.resolve(JSObject().put("status", "no_distributor"))
                    return@tryUseCurrentOrDefaultDistributor
                }
                // (Re-)register: the distributor answers with onNewEndpoint,
                // which emits `pushEndpoint` (also when it's unchanged, so the
                // daemon learns about this session's user).
                UnifiedPush.register(activity, INSTANCE_DEFAULT, "Kiln", null)
                val o = JSObject()
                if (known != null) {
                    o.put("status", "registered").put("endpoint", known)
                    val keys = KilnStore.pushKeys(activity)
                    emitEndpoint(known, keys?.first, keys?.second)
                } else {
                    o.put("status", "registering")
                }
                invoke.resolve(o)
            }
        }
    }

    @Command
    fun unregisterPush(invoke: Invoke) {
        UnifiedPush.unregister(activity, INSTANCE_DEFAULT)
        KilnStore.clear(activity)
        invoke.resolve()
    }

    // ------------------------------------------------------------ speech

    @Command
    fun startListening(invoke: Invoke) {
        if (getPermissionState("microphone") != PermissionState.GRANTED) {
            requestPermissionForAlias("microphone", invoke, "microphonePermission")
            return
        }
        startRecognizer(invoke)
    }

    @PermissionCallback
    private fun microphonePermission(invoke: Invoke) {
        if (getPermissionState("microphone") == PermissionState.GRANTED) {
            startRecognizer(invoke)
        } else {
            invoke.reject("Microphone permission denied", "permission")
        }
    }

    private fun startRecognizer(invoke: Invoke) {
        val args = invoke.parseArgs(ListenArgs::class.java)
        activity.runOnUiThread {
            if (!SpeechRecognizer.isRecognitionAvailable(activity)) {
                invoke.reject("Speech recognition is not available on this device", "unavailable")
                return@runOnUiThread
            }
            stopRecognizer()
            val r = SpeechRecognizer.createSpeechRecognizer(activity)
            recognizer = r
            r.setRecognitionListener(listener)
            val intent = Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH).apply {
                putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
                putExtra(RecognizerIntent.EXTRA_PARTIAL_RESULTS, true)
                putExtra(RecognizerIntent.EXTRA_CALLING_PACKAGE, activity.packageName)
                // Dictation: keep listening through short pauses.
                putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_COMPLETE_SILENCE_LENGTH_MILLIS, 4000L)
                putExtra(RecognizerIntent.EXTRA_SPEECH_INPUT_POSSIBLY_COMPLETE_SILENCE_LENGTH_MILLIS, 3000L)
                args.lang?.takeIf { it.isNotBlank() }?.let { putExtra(RecognizerIntent.EXTRA_LANGUAGE, it) }
            }
            r.startListening(intent)
            invoke.resolve()
        }
    }

    @Command
    fun stopListening(invoke: Invoke) {
        activity.runOnUiThread {
            stopRecognizer()
            invoke.resolve()
        }
    }

    private fun stopRecognizer() {
        recognizer?.let {
            try {
                it.cancel()
                it.destroy()
            } catch (_: Exception) {
            }
        }
        recognizer = null
    }

    private fun speech(type: String): JSObject = JSObject().put("type", type)

    private val listener = object : RecognitionListener {
        override fun onReadyForSpeech(params: Bundle?) {}
        override fun onBeginningOfSpeech() {}
        override fun onRmsChanged(rmsdB: Float) {
            // rmsdB is roughly -2..10; map to 0..1 and throttle to ~12/s.
            val now = System.currentTimeMillis()
            if (now - lastLevelAt < 80) return
            lastLevelAt = now
            val level = ((rmsdB + 2f) / 12f).coerceIn(0f, 1f).toDouble()
            trigger("speech", speech("level").put("level", level))
        }
        override fun onBufferReceived(buffer: ByteArray?) {}
        override fun onEndOfSpeech() {}
        override fun onError(error: Int) {
            // "No match" / speech timeout after some silence: not fatal in
            // dictation, the user just sends what we have.
            if (error == SpeechRecognizer.ERROR_NO_MATCH || error == SpeechRecognizer.ERROR_SPEECH_TIMEOUT) {
                trigger("speech", speech("end"))
                return
            }
            trigger("speech", speech("error").put("code", error))
        }
        override fun onResults(results: Bundle?) {
            val text = results?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)?.firstOrNull() ?: ""
            if (text.isNotBlank()) trigger("speech", speech("final").put("text", text))
            trigger("speech", speech("end"))
        }
        override fun onPartialResults(partialResults: Bundle?) {
            val text = partialResults?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)?.firstOrNull() ?: return
            trigger("speech", speech("partial").put("text", text))
        }
        override fun onEvent(eventType: Int, params: Bundle?) {}
    }
}
