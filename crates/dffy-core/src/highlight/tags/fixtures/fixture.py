"""A module docstring."""
from .util import helper
import os

LIMIT = 10
name: str = "x"
a, b = 1, 2
__all__ = ["top"]


def top(x):
    def inner():
        pass
    local = 1
    return x


@decorator
async def fetch():
    pass


class Point(Base):
    """A point."""
    origin = None
    count: int = 0

    def __init__(self):
        self.x = 1

    @property
    def length(self):
        return 0

    class Inner:
        def deep(self):
            pass


if TYPE_CHECKING:
    def guarded():
        pass

try:
    import fast
except ImportError:
    FALLBACK = True

type Alias = list[int]
