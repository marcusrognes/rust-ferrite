package com.ferrite

import android.os.Bundle
import android.view.Gravity
import android.widget.TextView
import androidx.appcompat.app.AppCompatActivity

private const val HOST = "10.0.2.2" // emulator -> host loopback; change for real device
private const val PORT = 7543

class MainActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val tv = TextView(this).apply {
            textSize = 24f
            gravity = Gravity.CENTER
            text = "${FerriteLib.connect()}\n\nconnecting to $HOST:$PORT..."
        }
        setContentView(tv)
        Thread {
            val result = FerriteLib.ping(HOST, PORT)
            runOnUiThread { tv.text = "${FerriteLib.connect()}\n\n$result" }
        }.start()
    }
}
