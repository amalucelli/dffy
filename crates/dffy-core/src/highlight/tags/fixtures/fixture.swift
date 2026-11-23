import Foundation

let limit = 10
var counter = 0

func top(x: Int) -> Int {
    func inner() {}
    let local = 1
    return x
}

class Point: Shape {
    var x: Int = 0
    static let origin = Point()

    init() {}

    func area() -> Int {
        return 0
    }

    class Inner {
        func deep() {}
    }
}

struct Pair {
    func sum() -> Int { return 0 }
}

protocol Shape {
    func area() -> Int
}

extension Point {
    func moved() -> Point { return self }
}

enum Color {
    case red, green
    case blue

    func next() -> Color { return .red }
}

actor Counter {
    func increment() {}
}

typealias Id = String
