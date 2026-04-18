package com.ferrite

import android.content.Context
import android.media.MediaCodec
import android.media.MediaFormat
import android.net.Uri
import android.os.Bundle
import android.util.Log
import android.view.Gravity
import android.view.MotionEvent
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.Button
import android.widget.LinearLayout
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import com.google.mlkit.vision.codescanner.GmsBarcodeScannerOptions
import com.google.mlkit.vision.codescanner.GmsBarcodeScanning
import com.google.mlkit.vision.barcode.common.Barcode
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

private const val TAG = "ferrite"
private const val DEFAULT_HOST = "10.0.2.2"
private const val DEFAULT_PORT = 7543
private const val MIME = "video/avc"
private const val PREFS = "ferrite"
private const val KEY_HOST = "host"
private const val KEY_PORT = "port"

class MainActivity : AppCompatActivity(), SurfaceHolder.Callback, FrameCallback {
    private lateinit var status: TextView
    private lateinit var welcome: LinearLayout
    private lateinit var surfaceView: SurfaceView
    private var host: String = DEFAULT_HOST
    private var port: Int = DEFAULT_PORT
    private val streaming = AtomicBoolean(false)

    private val codecLock = Any()
    private var codec: MediaCodec? = null
    private var outputThread: Thread? = null
    private val running = AtomicBoolean(false)
    private val surfaceReady = AtomicBoolean(false)
    private val streamLoopActive = AtomicBoolean(false)
    @Volatile private var streamLoopShouldRun = false

    private val bytesIn = AtomicLong(0)
    private val framesRendered = AtomicLong(0)
    private var lastFpsTick = System.nanoTime()
    private var lastRenderedFrames = 0L

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        supportActionBar?.hide()

        val prefs = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        host = prefs.getString(KEY_HOST, DEFAULT_HOST) ?: DEFAULT_HOST
        port = prefs.getInt(KEY_PORT, DEFAULT_PORT)

        // Welcome screen — shown when not streaming. Big "Ferrite" title,
        // status line, scan-QR button. Hidden while streaming so the surface
        // gets the full window.
        val title = TextView(this).apply {
            text = "Ferrite"
            textSize = 48f
            gravity = Gravity.CENTER
        }
        status = TextView(this).apply {
            textSize = 18f
            gravity = Gravity.CENTER
            text = "starting..."
        }
        val savedTarget = TextView(this).apply {
            textSize = 14f
            gravity = Gravity.CENTER
            text = "saved Wi-Fi target: $host:$port\nor plug in USB"
            alpha = 0.6f
        }
        val scanBtn = Button(this).apply {
            text = "Scan QR"
            setOnClickListener { scanQr() }
        }
        welcome = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            setPadding(48, 48, 48, 48)
            setBackgroundColor(android.graphics.Color.BLACK)
            addView(title, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT)
                .apply { bottomMargin = 32 })
            addView(status, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT)
                .apply { bottomMargin = 16 })
            addView(savedTarget, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT)
                .apply { bottomMargin = 32 })
            addView(scanBtn, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        }

        // SurfaceView must stay VISIBLE for the underlying surface to exist;
        // we just stack the welcome layout on top while not streaming and
        // hide the welcome once frames start arriving.
        surfaceView = SurfaceView(this).apply {
            holder.addCallback(this@MainActivity)
            setOnTouchListener { v, e -> handleTouch(v.width, v.height, e) }
            setOnHoverListener { v, e -> handleHover(v.width, v.height, e) }
            setOnGenericMotionListener { v, e -> handleHover(v.width, v.height, e) }
        }

        val root = android.widget.FrameLayout(this).apply {
            addView(surfaceView, android.widget.FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
            addView(welcome, android.widget.FrameLayout.LayoutParams(MATCH_PARENT, MATCH_PARENT))
        }
        setContentView(root)
    }

    private fun setStreamingUi(on: Boolean) {
        if (!streaming.compareAndSet(!on, on)) return
        runOnUiThread {
            welcome.visibility = if (on) android.view.View.GONE else android.view.View.VISIBLE
            applyImmersive(on)
        }
    }

    private fun applyImmersive(on: Boolean) {
        val controller = androidx.core.view.WindowCompat.getInsetsController(window, window.decorView)
        if (on) {
            androidx.core.view.WindowCompat.setDecorFitsSystemWindows(window, false)
            controller.hide(androidx.core.view.WindowInsetsCompat.Type.systemBars())
            controller.systemBarsBehavior =
                androidx.core.view.WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        } else {
            androidx.core.view.WindowCompat.setDecorFitsSystemWindows(window, true)
            controller.show(androidx.core.view.WindowInsetsCompat.Type.systemBars())
        }
    }

    private fun scanQr() {
        val options = GmsBarcodeScannerOptions.Builder()
            .setBarcodeFormats(Barcode.FORMAT_QR_CODE)
            .build()
        val scanner = GmsBarcodeScanning.getClient(this, options)
        scanner.startScan()
            .addOnSuccessListener { barcode ->
                val raw = barcode.rawValue ?: return@addOnSuccessListener
                handleScan(raw)
            }
            .addOnFailureListener { e ->
                Log.w(TAG, "scan failed", e)
                status.text = "scan err: ${e.message}"
            }
            .addOnCanceledListener {
                Log.i(TAG, "scan canceled")
            }
    }

    private fun handleScan(raw: String) {
        val uri = try { Uri.parse(raw) } catch (e: Throwable) {
            status.text = "bad QR: $raw"
            return
        }
        val newHost = uri.host
        if (newHost.isNullOrEmpty()) {
            status.text = "QR has no host: $raw"
            return
        }
        val newPort = if (uri.port > 0) uri.port else DEFAULT_PORT

        getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit()
            .putString(KEY_HOST, newHost)
            .putInt(KEY_PORT, newPort)
            .apply()

        // Tear down current connection + loop so the recreate gets a clean state.
        stopStreamLoop()
        recreate()
    }

    override fun surfaceCreated(holder: SurfaceHolder) {
        surfaceReady.set(true)
        startStreamIfNeeded()
    }

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {}

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        surfaceReady.set(false)
        stopCodec()
    }

    override fun onDestroy() {
        super.onDestroy()
        stopStreamLoop()
        stopCodec()
    }

    private fun startStreamIfNeeded() {
        if (!streamLoopActive.compareAndSet(false, true)) return
        streamLoopShouldRun = true
        val name = "${android.os.Build.MANUFACTURER} ${android.os.Build.MODEL}".trim()
        val metrics = resources.displayMetrics
        val w = metrics.widthPixels
        val ht = metrics.heightPixels
        Thread(null, {
            try {
                while (streamLoopShouldRun) {
                    val savedHost = host
                    val savedPort = port
                    // Re-probe each iteration so plugging in USB or starting
                    // the host afterwards is picked up automatically.
                    val target = when {
                        probeReachable("127.0.0.1", DEFAULT_PORT, 300) ->
                            "127.0.0.1" to DEFAULT_PORT
                        probeReachable(savedHost, savedPort, 500) ->
                            savedHost to savedPort
                        else -> null
                    }
                    if (target == null) {
                        runOnUiThread { status.text = "waiting for host..." }
                        Thread.sleep(2000)
                        continue
                    }
                    val (h, p) = target
                    runOnUiThread { status.text = "connecting $h:$p..." }
                    try {
                        FerriteLib.stream(h, p, name, w, ht, this)
                        runOnUiThread { status.text = "disconnected, retrying..." }
                    } catch (e: Throwable) {
                        Log.w(TAG, "stream ended: ${e.message}")
                        runOnUiThread { status.text = "err: ${e.message}, retrying..." }
                    }
                    setStreamingUi(false)
                    if (streamLoopShouldRun) Thread.sleep(1500)
                }
            } finally {
                streamLoopActive.set(false)
            }
        }, "ferrite-stream").start()
    }

    private fun stopStreamLoop() {
        streamLoopShouldRun = false
        try { FerriteLib.disconnect() } catch (_: Throwable) {}
    }

    private fun probeReachable(host: String, port: Int, timeoutMs: Int): Boolean {
        return try {
            java.net.Socket().use { s ->
                s.connect(java.net.InetSocketAddress(host, port), timeoutMs)
                true
            }
        } catch (_: Throwable) {
            false
        }
    }

    // Called from the JNI thread.
    override fun onFrame(data: ByteArray, width: Int, height: Int, format: Int) {
        if (format != FORMAT_H264) {
            Log.w(TAG, "unsupported format $format")
            return
        }
        setStreamingUi(true)
        synchronized(codecLock) {
            if (!surfaceReady.get()) return
            val c = codec ?: createCodecLocked(width, height) ?: return
            // Retry briefly if the decoder hasn't returned an input buffer yet.
            // Dropping a chunk here corrupts the H.264 stream and produces
            // visible artifacts until the next IDR. We'd rather backpressure
            // the network reader (which back-pressures TCP and the host
            // encoder) than lose data.
            var idx = -1
            val deadlineNs = System.nanoTime() + 200_000_000L // 200 ms
            while (idx < 0 && System.nanoTime() < deadlineNs) {
                idx = try {
                    c.dequeueInputBuffer(20_000L)
                } catch (e: IllegalStateException) {
                    Log.e(TAG, "dequeueInputBuffer failed", e)
                    return
                }
            }
            if (idx < 0) {
                Log.w(TAG, "decoder input buffer unavailable after 200ms; dropping ${data.size}B")
                return
            }
            val buf = c.getInputBuffer(idx) ?: return
            buf.clear()
            buf.put(data)
            c.queueInputBuffer(idx, 0, data.size, 0, 0)
            bytesIn.addAndGet(data.size.toLong())
        }
    }

    private fun createCodecLocked(width: Int, height: Int): MediaCodec? {
        return try {
            val holder = surfaceView.holder
            if (!holder.surface.isValid) return null
            val c = MediaCodec.createDecoderByType(MIME)
            val fmt = MediaFormat.createVideoFormat(MIME, width, height)
            // KEY_LOW_LATENCY hint (API 30+); harmless on older devices.
            if (android.os.Build.VERSION.SDK_INT >= 30) {
                fmt.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            }
            c.configure(fmt, holder.surface, null, 0)
            c.start()
            codec = c
            running.set(true)
            outputThread = Thread(null, { outputLoop(c) }, "ferrite-output").also { it.start() }
            Log.i(TAG, "codec configured ${width}x${height}")
            c
        } catch (e: Throwable) {
            Log.e(TAG, "codec create failed", e)
            null
        }
    }

    private fun outputLoop(c: MediaCodec) {
        val info = MediaCodec.BufferInfo()
        while (running.get()) {
            val idx = try {
                c.dequeueOutputBuffer(info, 20_000L)
            } catch (e: IllegalStateException) {
                Log.e(TAG, "dequeueOutputBuffer failed", e)
                return
            }
            when {
                idx >= 0 -> {
                    c.releaseOutputBuffer(idx, true)
                    val n = framesRendered.incrementAndGet()
                    val now = System.nanoTime()
                    val elapsed = now - lastFpsTick
                    if (elapsed >= 1_000_000_000L) {
                        val fps = (n - lastRenderedFrames).toDouble() * 1e9 / elapsed
                        val kbs = bytesIn.getAndSet(0).toDouble() / 1024.0 * 1e9 / elapsed
                        lastFpsTick = now
                        lastRenderedFrames = n
                        runOnUiThread {
                            status.text = "%.1f fps — %.0f KB/s in — rendered #%d"
                                .format(fps, kbs, n)
                        }
                    }
                }
                idx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED ->
                    Log.i(TAG, "output format: ${c.outputFormat}")
                else -> {} // INFO_TRY_AGAIN_LATER or deprecated
            }
        }
    }

    private fun handleTouch(viewW: Int, viewH: Int, e: MotionEvent): Boolean {
        if (viewW <= 0 || viewH <= 0) return false

        // Pen / eraser → single Pointer with pressure + proximity. Pen events
        // never have pointerCount > 1 in practice so no MT split needed.
        val firstTool = toolFor(e)
        if (firstTool != 0) {
            val x = (e.x / viewW).coerceIn(0f, 1f)
            val y = (e.y / viewH).coerceIn(0f, 1f)
            val pressed = when (e.actionMasked) {
                MotionEvent.ACTION_DOWN, MotionEvent.ACTION_MOVE -> true
                MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> false
                else -> return false
            }
            val pressure = e.pressure.coerceIn(0f, 1f)
            try {
                FerriteLib.sendPointer(x, y, pressed, pressure, firstTool, true)
            } catch (t: Throwable) {
                Log.w(TAG, "sendPointer failed", t)
            }
            return true
        }

        // Finger touches → snapshot of every currently-down pointer. On
        // ACTION_(POINTER_)UP the lifted pointer is still in the event but
        // about to release, so exclude it from the snapshot.
        val released: Int = when (e.actionMasked) {
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> -2 // all up
            MotionEvent.ACTION_POINTER_UP -> e.actionIndex
            else -> -1
        }
        val n = e.pointerCount
        val capacity = if (released == -2) 0 else n
        val ids = IntArray(capacity)
        val xs = FloatArray(capacity)
        val ys = FloatArray(capacity)
        var k = 0
        if (released != -2) {
            for (i in 0 until n) {
                if (i == released) continue
                ids[k] = e.getPointerId(i)
                xs[k] = (e.getX(i) / viewW).coerceIn(0f, 1f)
                ys[k] = (e.getY(i) / viewH).coerceIn(0f, 1f)
                k++
            }
        }
        // Trim if we excluded one.
        val actualIds = if (k == capacity) ids else ids.copyOf(k)
        val actualXs = if (k == capacity) xs else xs.copyOf(k)
        val actualYs = if (k == capacity) ys else ys.copyOf(k)
        try {
            FerriteLib.sendTouches(actualIds, actualXs, actualYs)
        } catch (t: Throwable) {
            Log.w(TAG, "sendTouches failed", t)
        }
        return true
    }

    // S-Pen sends ACTION_HOVER_* while in proximity but not touching.
    private fun handleHover(viewW: Int, viewH: Int, e: MotionEvent): Boolean {
        if (viewW <= 0 || viewH <= 0) return false
        val x = (e.x / viewW).coerceIn(0f, 1f)
        val y = (e.y / viewH).coerceIn(0f, 1f)
        val inRange = e.actionMasked != MotionEvent.ACTION_HOVER_EXIT
        val tool = toolFor(e)
        try {
            FerriteLib.sendPointer(x, y, false, 0f, tool, inRange)
        } catch (t: Throwable) {
            Log.w(TAG, "sendPointer hover failed", t)
        }
        return true
    }

    private fun toolFor(e: MotionEvent): Int = when (e.getToolType(0)) {
        MotionEvent.TOOL_TYPE_STYLUS -> 1
        MotionEvent.TOOL_TYPE_ERASER -> 2
        else -> 0
    }

    private fun stopCodec() {
        synchronized(codecLock) {
            running.set(false)
            outputThread?.join(200)
            outputThread = null
            codec?.let {
                try { it.stop() } catch (_: Throwable) {}
                try { it.release() } catch (_: Throwable) {}
            }
            codec = null
        }
    }
}
