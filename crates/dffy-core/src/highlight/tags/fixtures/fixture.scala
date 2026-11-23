package com.acme.geo

import scala.util.Try

val Limit = 10

def top(a: Int): Int =
  def inner() = 1
  a

trait Shape:
  def area: Int
  def name: String = "shape"

abstract class Base

case class Point(x: Int, y: Int) extends Shape:
  val size = 1
  def area: Int = 0

object Point:
  def origin: Point = Point(0, 0)

enum Color:
  case Red, Green

type Id = String

given ordering: Ordering[Int] = Ordering.Int

extension (p: Point)
  def moved: Point = p
