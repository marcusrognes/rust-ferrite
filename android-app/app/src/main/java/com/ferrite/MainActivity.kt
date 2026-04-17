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
    private lateinit var surfaceView: SurfaceView
    private var host: String = DEFAULT_HOST
    private var port: Int = DEFAULT_PORT

    private val codecLock = Any()
    private var codec: MediaCodec? = null
    private var outputThread: Thread? = null
    private val running = AtomicBoolean(false)
    private val surfaceReady = AtomicBoolean(false)
    private val streamStarted = AtomicBoolean(false)

    private val bytesIn = AtomicLong(0)
    private val framesRendered = AtomicLong(0)
    private var lastFpsTick = System.nanoTime()
    private var lastRenderedFrames = 0L

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        val prefs = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        host = prefs.getString(KEY_HOST, DEFAULT_HOST) ?: DEFAULT_HOST
        port = prefs.getInt(KEY_PORT, DEFAULT_PORT)

        status = TextView(this).apply {
            textSize = 14f
            gravity = Gravity.CENTER
            text = "$host:$port"
        }
        val scanBtn = Button(this).apply {
            text = "Scan QR"
            setOnClickListener { scanQr() }
        }
        val toolbar = LinearLayout(this).apply {
            orientation = LinearLayout.HORIZONTAL
            addView(status, LinearLayout.LayoutParams(0, WRAP_CONTENT, 1f))
            addView(scanBtn, LinearLayout.LayoutParams(WRAP_CONTENT, WRAP_CONTENT))
        }
        surfaceView = SurfaceView(this).apply {
            holder.addCallback(this@MainActivity)
            setOnTouchListener { v, e -> handleTouch(v.width, v.height, e) }
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(toolbar, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(surfaceView, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        }
        setContentView(root)
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

        // Tear down current connection so the recreate gets a clean state.
        try { FerriteLib.disconnect() } catch (_: Throwable) {}
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
        stopCodec()
    }

    private fun startStreamIfNeeded() {
        if (streamStarted.compareAndSet(false, true)) {
            val h = host
            val p = port
            runOnUiThread { status.text = "connecting $h:$p..." }
            Thread(null, {
                try {
                    FerriteLib.stream(h, p, this)
                } catch (e: Throwable) {
                    Log.e(TAG, "stream error", e)
                    runOnUiThread { status.text = "err: ${e.message}" }
                }
            }, "ferrite-stream").start()
        }
    }

    // Called from the JNI thread.
    override fun onFrame(data: ByteArray, width: Int, height: Int, format: Int) {
        if (format != FORMAT_H264) {
            Log.w(TAG, "unsupported format $format")
            return
        }
        synchronized(codecLock) {
            if (!surfaceReady.get()) return
            val c = codec ?: createCodecLocked(width, height) ?: return
            val idx = try {
                c.dequeueInputBuffer(20_000L)
            } catch (e: IllegalStateException) {
                Log.e(TAG, "dequeueInputBuffer failed", e)
                return
            }
            if (idx < 0) return
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
        val x = (e.x / viewW).coerceIn(0f, 1f)
        val y = (e.y / viewH).coerceIn(0f, 1f)
        val pressed = when (e.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_MOVE -> true
            MotionEvent.ACTION_UP, MotionEvent.ACTION_CANCEL -> false
            else -> return false
        }
        // pressure: stylus reports real values; finger usually reports 1.0 when down.
        val pressure = e.pressure.coerceIn(0f, 1f)
        val tool = when (e.getToolType(0)) {
            MotionEvent.TOOL_TYPE_STYLUS -> 1   // PointerTool::Pen
            MotionEvent.TOOL_TYPE_ERASER -> 2   // PointerTool::Eraser
            else -> 0                            // PointerTool::Finger
        }
        try {
            FerriteLib.sendPointer(x, y, pressed, pressure, tool)
        } catch (t: Throwable) {
            Log.w(TAG, "sendPointer failed", t)
        }
        return true
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
