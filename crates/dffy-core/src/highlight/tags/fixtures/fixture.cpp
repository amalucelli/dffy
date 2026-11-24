#include "geo.hpp"
#define SQUARE(x) ((x) * (x))

namespace geo {

constexpr int kLimit = 10;

class Point : public Base {
public:
    Point();
    int area() const { return 0; }
    virtual void draw() = 0;
    static Point origin();
    int x;
};

struct Pair {
    int a, b;
    int sum() { return a + b; }
};

int Point::area2() const {
    struct Local {};
    return 0;
}

Point::Point() : x(0) {}

Point::~Point() {}

bool operator==(const Point& a, const Point& b) { return true; }

template <typename T>
T max(T a, T b) {
    return a;
}

int proto(int a);

enum class Color { Red, Green };

using Id = int;
typedef unsigned long ulong_t;

namespace detail {
void helper() {}
}

}  // namespace geo

template <typename T>
class Box {
    T get() { return value; }
    T value;
};

void geo::Pair::reset() {}

struct Shape {
    virtual double area() const = 0;
    Shape() {}
    int sides;
};
