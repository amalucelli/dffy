package com.acme.geo

import com.acme.util.parse

const val LIMIT = 10
val origin = Point(0)
var counter = 0

fun top(x: Int): Int {
    fun inner() {}
    val local = 1
    return x
}

fun Int.twice() = this * 2

class Point(val x: Int) : Shape {
    val size: Int = 1

    constructor() : this(0)

    override fun area(): Int = 0

    companion object {
        fun origin() = Point(0)
    }

    class Inner {
        fun deep() {}
    }
}

interface Shape {
    fun area(): Int

    fun name(): String = "shape"
}

abstract class Base {
    abstract fun draw()
}

data class Pair(val a: Int, val b: Int)

enum class Color { RED, GREEN }

object Registry {
    fun register() {}
}

typealias Id = String
