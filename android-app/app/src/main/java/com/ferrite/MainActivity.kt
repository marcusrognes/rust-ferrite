package com.ferrite

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.os.Bundle
import android.view.Gravity
import android.view.ViewGroup.LayoutParams.MATCH_PARENT
import android.view.ViewGroup.LayoutParams.WRAP_CONTENT
import android.widget.ImageView
import android.widget.LinearLayout
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicLong

private const val HOST = "10.0.2.2" // emulator -> host loopback; change for real device
private const val PORT = 7543

class MainActivity : AppCompatActivity(), FrameCallback {
    private lateinit var status: TextView
    private lateinit var image: ImageView

    // Reused RGBA bitmap — recreated only if size changes.
    private var rgbaBitmap: Bitmap? = null

    private val frames = AtomicLong(0)
    private var lastFpsTick = System.nanoTime()
    private var lastFpsFrames = 0L
    private var lastPayloadBytes = 0L

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        status = TextView(this).apply {
            textSize = 16f
            gravity = Gravity.CENTER
            text = "connecting to $HOST:$PORT..."
        }
        image = ImageView(this).apply {
            scaleType = ImageView.ScaleType.FIT_CENTER
        }

        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            addView(status, LinearLayout.LayoutParams(MATCH_PARENT, WRAP_CONTENT))
            addView(image, LinearLayout.LayoutParams(MATCH_PARENT, 0, 1f))
        }
        setContentView(root)

        Thread(null, {
            try {
                FerriteLib.stream(HOST, PORT, this)
            } catch (e: Throwable) {
                runOnUiThread { status.text = "err: ${e.message}" }
            }
        }, "ferrite-stream").start()
    }

    // Called from the JNI thread — NOT the UI thread.
    override fun onFrame(data: ByteArray, width: Int, height: Int, format: Int) {
        val bmp: Bitmap = when (format) {
            FORMAT_JPEG -> BitmapFactory.decodeByteArray(data, 0, data.size) ?: return
            FORMAT_RGBA8 -> {
                val existing = rgbaBitmap?.takeIf { it.width == width && it.height == height }
                    ?: Bitmap.createBitmap(width, height, Bitmap.Config.ARGB_8888)
                        .also { rgbaBitmap = it }
                existing.copyPixelsFromBuffer(ByteBuffer.wrap(data))
                existing
            }
            else -> return
        }

        val n = frames.incrementAndGet()
        lastPayloadBytes = data.size.toLong()
        val now = System.nanoTime()
        val elapsedNs = now - lastFpsTick
        val statusText: String? = if (elapsedNs >= 1_000_000_000L) {
            val fps = (n - lastFpsFrames).toDouble() * 1e9 / elapsedNs
            lastFpsTick = now
            lastFpsFrames = n
            val kb = lastPayloadBytes / 1024
            val fmtName = if (format == FORMAT_JPEG) "jpeg" else "rgba"
            "${width}x${height} $fmtName — %.1f fps — %d KB/frame — #%d".format(fps, kb, n)
        } else null

        runOnUiThread {
            image.setImageBitmap(bmp)
            if (statusText != null) status.text = statusText
        }
    }
}
