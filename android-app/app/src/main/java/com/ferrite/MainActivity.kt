package com.ferrite

import android.media.MediaCodec
import android.media.MediaFormat
import android.os.Bundle
import android.util.Log
import android.view.Gravity
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.LinearLayout
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

private const val TAG = "ferrite"
private const val HOST = "10.0.2.2"
private const val PORT = 7543
private const val MIME = "video/avc"

class MainActivity : AppCompatActivity(), SurfaceHolder.Callback, FrameCallback {
    private lateinit var status: TextView
    private lateinit var surfaceView: SurfaceView

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
        status = TextView(this).apply {
            textSize = 14f
            gravity = Gravity.CENTER
            text = "waiting for surface..."
        }
        surfaceView = SurfaceView(this).apply {
            holder.addCallback(this@MainActivity)
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(status, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(surfaceView, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        }
        setContentView(root)
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
            runOnUiThread { status.text = "connecting $HOST:$PORT..." }
            Thread(null, {
                try {
                    FerriteLib.stream(HOST, PORT, this)
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
