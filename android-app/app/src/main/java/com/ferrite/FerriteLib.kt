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
     * Opens a TCP connection to host:port, sends `Hello { deviceName, width,
     * height }`, then blocks calling `cb.onFrame(bytes, w, h, format)` once
     * per incoming video frame until the connection errors. Call from a
     * background thread — this blocks indefinitely.
     *
     * `deviceName` shows up as the virtual monitor name (in virtual mode) and
     * is suffixed onto the touchscreen + pen device names in libinput.
     * `width`/`height` are the desired virtual-monitor pixels (ignored in
     * mirror mode).
     *
     * Throws RuntimeException on I/O or protocol error.
     */
    external fun stream(
        host: String,
        port: Int,
        deviceName: String,
        width: Int,
        height: Int,
        cb: FrameCallback,
    )

    /**
     * Push a pointer event to the host over the currently-active `stream()`
     * socket. `x, y` ∈ [0,1] within the view; `pressure` ∈ [0,1]; `tool` is
     * 0=Finger, 1=Pen, 2=Eraser. No-op if there's no live stream.
     */
    external fun sendPointer(
        x: Float, y: Float,
        pressed: Boolean,
        pressure: Float,
        tool: Int,
        inRange: Boolean,
    )

    /** Aborts the current `stream()` blocking call by closing the socket. */
    external fun disconnect()
}
