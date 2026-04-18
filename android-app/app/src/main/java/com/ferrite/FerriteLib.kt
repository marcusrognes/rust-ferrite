package com.ferrite

interface FrameCallback {
    /**
     * @param format 0 = raw RGBA (R,G,B,A bytes, size = w*h*4)
     *               1 = JPEG (complete encoded byte stream)
     *               2 = H.264 (Annex-B bytes, one access unit per call)
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
     * Opens a TCP connection to host:port, sends Hello, then blocks running
     * the stream protocol until the socket errors.
     *
     * `deviceName` shows up as the virtual monitor name (virtual mode) and is
     * suffixed onto the touchscreen + pen device names in libinput.
     * `width` / `height` are the desired virtual-monitor pixels (ignored in
     * mirror mode).
     *
     * Throws RuntimeException on I/O or protocol error.
     */
    external fun streamTcp(
        host: String,
        port: Int,
        deviceName: String,
        width: Int,
        height: Int,
        cb: FrameCallback,
    )

    /**
     * AOA transport: same protocol as [streamTcp] but over a raw UNIX fd
     * obtained from `UsbManager.openAccessory(...).detachFd()`. Takes
     * ownership of the fd — do not close it on the Kotlin side.
     */
    external fun streamFd(
        fd: Int,
        deviceName: String,
        width: Int,
        height: Int,
        cb: FrameCallback,
    )

    /**
     * Push a pointer event to the host over the currently-active stream.
     * `x, y` ∈ [0,1] within the view; `pressure` ∈ [0,1]; `tool` is
     * 0=Finger, 1=Pen, 2=Eraser. No-op if there's no live stream.
     */
    external fun sendPointer(
        x: Float, y: Float,
        pressed: Boolean,
        pressure: Float,
        tool: Int,
        inRange: Boolean,
    )

    /**
     * Push a multi-touch snapshot. Parallel arrays of currently-down finger
     * ids and normalized [0,1] positions. Empty arrays = all fingers up.
     */
    external fun sendTouches(ids: IntArray, xs: FloatArray, ys: FloatArray)

    /** Aborts the current blocking stream call. */
    external fun disconnect()
}

/**
 * A way to reach the ferrite-host. Add a new variant + implementation of
 * [run] when wiring up a new transport.
 */
sealed class Transport {
    abstract fun run(deviceName: String, width: Int, height: Int, cb: FrameCallback)
    abstract fun label(): String

    /** Wi-Fi (LAN) or adb-reverse USB — both are TCP, they just differ in host. */
    data class Tcp(val host: String, val port: Int) : Transport() {
        override fun run(deviceName: String, width: Int, height: Int, cb: FrameCallback) =
            FerriteLib.streamTcp(host, port, deviceName, width, height, cb)
        override fun label() = "$host:$port"
    }

    /** Android Open Accessory — raw bulk fd from UsbManager. */
    data class Aoa(val fd: Int) : Transport() {
        override fun run(deviceName: String, width: Int, height: Int, cb: FrameCallback) =
            FerriteLib.streamFd(fd, deviceName, width, height, cb)
        override fun label() = "aoa fd=$fd"
    }
}
