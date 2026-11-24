const std = @import("std");

pub const limit = 10;
var counter: u32 = 0;

pub fn top(a: u32) u32 {
    const local = 1;
    return a + local;
}

pub const Point = struct {
    x: i32,
    y: i32 = 0,

    pub const origin = Point{ .x = 0 };

    pub fn area(self: Point) i32 {
        return self.x;
    }
};

const Color = enum {
    red,
    green,

    fn next(c: Color) Color {
        return c;
    }
};

const Value = union(enum) {
    int: i64,
    none,
};

test "point area" {
    _ = top(1);
}

const Outer = struct {
    const Inner = struct {
        const depth = 2;
    };
};
