<?php
namespace App\Models;

use App\Support\Helper;

const LIMIT = 10;

function top($a) {
    function inner() {}
    return $a;
}

interface Shape {
    public function area(): int;
}

trait Greets {
    public function greet() {}
}

abstract class Point extends Base implements Shape {
    const ORIGIN = 0;
    public int $x = 0;
    private static $count;

    public function __construct() {}

    public function area(): int {
        return 0;
    }

    abstract protected function draw();
}

enum Color: string {
    case Red = 'r';
    case Green = 'g';

    public function label(): string { return 'c'; }
}
