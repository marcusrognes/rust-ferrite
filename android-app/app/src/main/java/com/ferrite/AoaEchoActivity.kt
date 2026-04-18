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
        status.text = "connected fd=${fd.fd}; echoing + heartbeats"

        val input = FileInputStream(fd.fileDescriptor)
        val output = FileOutputStream(fd.fileDescriptor)
        // Shared lock so heartbeat writes don't interleave with echo writes.
        val outputLock = Any()

        // Echo thread: read from host, write back. Synchronized with heartbeat
        // thread on outputLock — each write_all is atomic relative to the
        // other path.
        worker = Thread({
            val buf = ByteArray(64 * 1024)
            var total = 0L
            try {
                while (!Thread.currentThread().isInterrupted) {
                    val n = input.read(buf)
                    if (n < 0) break
                    if (n == 0) continue
                    synchronized(outputLock) {
                        output.write(buf, 0, n)
                        output.flush()
                    }
                    total += n
                }
                Log.i(TAG, "echo loop exited after $total bytes")
            } catch (e: Throwable) {
                Log.w(TAG, "echo ended", e)
                runOnUiThread { status.text = "echo ended: ${e.message}" }
            }
        }, "aoa-echo").also { it.start() }

        // Heartbeat thread: writes a 2-byte marker (0xFF 0xFF) every 100ms.
        // The host test pattern is restricted to bytes 0..0xFE so heartbeats
        // are trivially separable from echoed content.
        Thread({
            val hb = byteArrayOf(0xFF.toByte(), 0xFF.toByte())
            try {
                while (!Thread.currentThread().isInterrupted) {
                    synchronized(outputLock) {
                        output.write(hb)
                        output.flush()
                    }
                    Thread.sleep(100)
                }
            } catch (_: InterruptedException) {
            } catch (e: Throwable) {
                Log.w(TAG, "heartbeat ended", e)
            }
        }, "aoa-heartbeat").start()
    }

    companion object {
        private const val TAG = "ferrite-echo"
    }
}
