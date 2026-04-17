package com.ferrite

object FerriteLib {
    init { System.loadLibrary("ferrite_android") }
    external fun connect(): String
    external fun ping(host: String, port: Int): String
}
