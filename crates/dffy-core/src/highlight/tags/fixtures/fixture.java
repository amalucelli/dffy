package com.example.geo;

import java.util.List;

public class Point extends Base implements Shape {
    public static final int LIMIT = 10;
    private int x;

    public Point() {
        this.x = 0;
    }

    public int area() {
        Runnable r = new Runnable() {
            public void run() {}
        };
        return 0;
    }

    abstract void draw();

    static class Inner {
        void deep() {}
    }
}

interface Shape {
    int area();

    default String name() {
        return "shape";
    }
}

enum Color {
    RED, GREEN;

    Color next() {
        return RED;
    }
}

record Pair(int a, int b) {
    int sum() {
        return a + b;
    }
}

@interface Marker {
    String value();
}
