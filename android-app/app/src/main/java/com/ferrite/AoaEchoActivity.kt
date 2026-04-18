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
import java.io.FileInputStream
import java.io.FileOutputStream

/**
 * Minimal AOA echo activity. Launched when the USB_ACCESSORY_ATTACHED intent
 * matches [res/xml/accessory_echo_filter.xml] (manufacturer = "co.dealdrive",
 * model = "FerriteEcho"). Reads any bytes the host writes and writes them
 * straight back. No framing, no MediaCodec, no other state.
 *
 * Pair with the `aoa-test` host binary.
 */
class AoaEchoActivity : AppCompatActivity() {
    private lateinit var status: TextView
    private var pfd: ParcelFileDescriptor? = null
    private var worker: Thread? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        supportActionBar?.hide()

        status = TextView(this).apply {
            textSize = 18f
            gravity = Gravity.CENTER
            text = "AOA echo — waiting for host"
            setTextColor(0xFFFFFFFF.toInt())
        }
        val root = LinearLayout(this).apply {
            orientation = LinearLayout.VERTICAL
            gravity = Gravity.CENTER
            setBackgroundColor(0xFF000000.toInt())
            addView(status)
        }
        setContentView(root)

        startEcho(intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        startEcho(intent)
    }

    override fun onDestroy() {
        super.onDestroy()
        worker?.interrupt()
        pfd?.close()
    }

    private fun startEcho(intent: Intent?) {
        val mgr = getSystemService(Context.USB_SERVICE) as UsbManager
        val acc: UsbAccessory = intent?.getParcelableExtra(UsbManager.EXTRA_ACCESSORY)
            ?: mgr.accessoryList?.firstOrNull()
            ?: run {
                status.text = "no accessory"
                return
            }

        val fd = mgr.openAccessory(acc)
        if (fd == null) {
            status.text = "openAccessory returned null"
            return
        }
        pfd = fd
        status.text = "connected fd=${fd.fd}; echoing"

        val input = FileInputStream(fd.fileDescriptor)
        val output = FileOutputStream(fd.fileDescriptor)
        worker = Thread({
            val buf = ByteArray(64 * 1024)
            var total = 0L
            try {
                while (!Thread.currentThread().isInterrupted) {
                    val n = input.read(buf)
                    if (n < 0) break
                    if (n == 0) continue
                    output.write(buf, 0, n)
                    output.flush()
                    total += n
                    val msg = "echoed $total bytes"
                    runOnUiThread { status.text = msg }
                    Log.i(TAG, msg)
                }
            } catch (e: Throwable) {
                Log.w(TAG, "echo ended", e)
                val msg = "disconnected: ${e.message}"
                runOnUiThread { status.text = msg }
            }
        }, "aoa-echo").also { it.start() }
    }

    companion object {
        private const val TAG = "ferrite-echo"
    }
}
