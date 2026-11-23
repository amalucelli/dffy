package geo

import "fmt"

const Limit = 10

const (
	Red Color = iota
	Green
)

var Origin = Point{}

var (
	count int
	debug = false
)

type Point struct {
	X, Y int
}

type Shape interface {
	Area() float64
}

type Color int

type Alias = Point

type (
	Pair  [2]int
	Empty struct{}
)

func New(x int) *Point {
	local := 1
	_ = local
	return &Point{X: x}
}

func (p *Point) Move(dx int) {
	fmt.Println(dx)
}

func (p Pair[T]) First() T {
	return p[0]
}

func Max[T int | float64](a, b T) T {
	return a
}

func init() {}
