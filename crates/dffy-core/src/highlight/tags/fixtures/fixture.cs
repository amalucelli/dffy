using System;

namespace Acme.Geo
{
    public class Point : IShape
    {
        public const int Limit = 10;
        private int x;
        public int X { get; set; }

        public Point() { }

        public int Area() => 0;

        public abstract void Draw();

        partial void Hook();

        public event EventHandler Moved;

        class Inner
        {
            void Deep() { }
        }
    }

    public interface IShape
    {
        int Area();
    }

    public struct Pair
    {
        public int Sum() { return 0; }
    }

    public enum Color { Red, Green = 2 }

    public record Person(string Name);

    public delegate void Handler(int x);
}

namespace Acme.Other;

static class Ext
{
    public static int Twice(this int x) { return x * 2; }
}
