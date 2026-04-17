package com.ferrite

interface FrameCallback {
    /**
     * @param format 0 = raw RGBA (R,G,B,A bytes, size = w*h*4)
     *               1 = JPEG (complete encoded byte stream)
     */
    fun onFrame(data: ByteArray, width: Int, height: Int, format: Int)
}

const val FORMAT_RGBA8 = 0
const val FORMAT_JPEG = 1
const val FORMAT_H264 = 2

object FerriteLib {
    init { System.loadLibrary("ferrite_android") }
    external fun connect(): String
    /**
     * Opens a TCP connection to host:port and blocks, calling
     * `cb.onFrame(bytes, w, h, format)` once per incoming video frame until the
     * connection errors. Call from a background thread — this blocks indefinitely.
     *
     * Throws RuntimeException on I/O or protocol error.
     */
    external fun stream(host: String, port: Int, cb: FrameCallback)
}
