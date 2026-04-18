package com.ferrite

import android.content.Context
import android.content.Intent
import android.hardware.usb.UsbAccessory
import android.hardware.usb.UsbManager
import android.os.Bundle
import android.os.ParcelFileDescriptor
import android.util.Log
import android.view.Gravity
import android.widget.LinearLayout
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity
import java.util.concurrent.atomic.AtomicInteger

/**
 * Full-protocol AOA test activity. Matches the accessory filter
 * `manufacturer=co.dealdrive`, `model=FerriteProto`. On attach, opens the
 * accessory and hands its fd straight to [FerriteLib.streamFd] — the same
 * JNI function MainActivity uses.
 *
 * Frames are counted, not decoded. The point of this activity is to exercise
 * the real wire protocol in isolation from MainActivity's permission /
 * reconnect / Wi-Fi machinery. If sessions here are reliable, the protocol
 * and JNI are clean and the main-app bug lives in MainActivity.
 */
class AoaProtoActivity : AppCompatActivity(), FrameCallback {
    private lateinit var status: TextView
    private var pfd: ParcelFileDescriptor? = null
    private var streamThread: Thread? = null
    private val frames = AtomicInteger(0)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        supportActionBar?.hide()

        status = TextView(this).apply {
            textSize = 18f
            gravity = Gravity.CENTER
            text = "AOA proto — waiting"
            setTextColor(0xFFFFFFFF.toInt())
        }
        setContentView(LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            setBackgroundColor(0xFF000000.toInt())
            addView(status)
        })

        startProto(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        startProto(intent)
    }

    override fun onDestroy() {
        super.onDestroy()
        streamThread?.interrupt()
        try { FerriteLib.disconnect() } catch (_: Throwable) {}
        pfd?.close()
    }

    override fun onFrame(data: ByteArray, width: Int, height: Int, format: Int) {
        val n = frames.incrementAndGet()
        if (n % 10 == 0) {
            val msg = "frames: $n (latest ${data.size}B)"
            Log.i(TAG, msg)
            runOnUiThread { status.text = msg }
        }
    }

    private fun startProto(intent: Intent?) {
        val mgr = getSystemService(Context.USB_SERVICE) as UsbManager
        val acc: UsbAccessory = intent?.getParcelableExtra(UsbManager.EXTRA_ACCESSORY)
            ?: mgr.accessoryList?.firstOrNull()
            ?: run {
                status.text = "no accessory"
                return
            }

        val fd = try {
            mgr.openAccessory(acc)
        } catch (t: Throwable) {
            status.text = "openAccessory threw: ${t.message}"
            return
        }
        if (fd == null) {
            status.text = "openAccessory returned null"
            return
        }
        pfd = fd
        val rawFd = fd.detachFd()
        status.text = "streaming (fd=$rawFd)"
        frames.set(0)

        streamThread = Thread({
            try {
                // Fixed device name / dimensions — the host test doesn't
                // care about these, it just needs a well-formed Hello.
                FerriteLib.streamFd(rawFd, "ProtoTest", 1920, 1080, this)
                Log.i(TAG, "streamFd returned cleanly")
            } catch (e: Throwable) {
                Log.w(TAG, "streamFd ended", e)
            }
            runOnUiThread { status.text = "ended after ${frames.get()} frames" }
        }, "aoa-proto").also { it.start() }
    }

    companion object {
        private const val TAG = "ferrite-proto"
    }
}
