"""Sample Python module for tree-sitter-python symbol extraction tests.

Fixture size target: >500 LOC with a known mix of declarations:
  - top-level functions (sync + async)
  - classes with methods (regular, static, classmethod)
  - decorators (stacked + attribute-based)
  - module-level constants
  - nested classes / nested functions
  - inheritance chains

The real counts are asserted in `tests/python_extraction.rs`. Keep the
declarations stable — tests lock on the minima.
"""

import os
import sys
from typing import Any, Callable, Dict, Iterable, List, Optional, Tuple


# ----- module constants -----------------------------------------------------

CONST_ALPHA = 1
CONST_BETA = 2
CONST_GAMMA = 3
CONST_DELTA = 4
CONST_EPSILON = 5
CONST_ZETA: int = 6
CONST_ETA: int = 7
CONST_THETA: str = "theta"
CONST_IOTA: float = 9.9
CONST_KAPPA: Tuple[int, ...] = (10, 11, 12)


# ----- top-level functions --------------------------------------------------

def top_alpha(a: int, b: int) -> int:
    """Sum two ints."""
    return a + b


def top_beta() -> None:
    """Do nothing."""
    return None


def top_gamma(xs: List[int]) -> int:
    total = 0
    for x in xs:
        total += x
    return total


def top_delta(x: Any) -> Any:
    return x


def top_epsilon(x: int, y: int = 0) -> int:
    if y == 0:
        return x
    return x * y


def top_zeta(*args: int) -> int:
    return sum(args)


def top_eta(**kwargs: Any) -> Dict[str, Any]:
    return dict(kwargs)


def top_theta(cb: Callable[[int], int], v: int) -> int:
    return cb(v)


def top_iota(it: Iterable[int]) -> List[int]:
    return [x + 1 for x in it]


def top_kappa(path: str) -> Optional[str]:
    if os.path.exists(path):
        return path
    return None


async def async_worker_one() -> int:
    return 1


async def async_worker_two(x: int) -> int:
    return x + 1


async def async_worker_three(xs: List[int]) -> int:
    return sum(xs)


# ----- decorated top-level functions ---------------------------------------

def simple_decorator(f: Callable) -> Callable:
    def inner(*a, **kw):
        return f(*a, **kw)
    return inner


def logging_decorator(f: Callable) -> Callable:
    def inner(*a, **kw):
        sys.stdout.write(f"call {f.__name__}\n")
        return f(*a, **kw)
    return inner


def caching_decorator(f: Callable) -> Callable:
    cache: Dict[Tuple[Any, ...], Any] = {}

    def inner(*a, **kw):
        key = (a, tuple(sorted(kw.items())))
        if key not in cache:
            cache[key] = f(*a, **kw)
        return cache[key]

    return inner


@simple_decorator
def decorated_one() -> int:
    return 1


@logging_decorator
def decorated_two() -> int:
    return 2


@caching_decorator
def decorated_three(x: int) -> int:
    return x * x


@simple_decorator
@logging_decorator
def decorated_four() -> str:
    return "four"


# ----- classes --------------------------------------------------------------

class Alpha:
    """Top-level class with instance + static + classmethod."""

    def __init__(self, x: int) -> None:
        self.x = x

    def method_a(self) -> int:
        return self.x

    def method_b(self, y: int) -> int:
        return self.x + y

    @staticmethod
    def static_method() -> str:
        return "alpha.static"

    @classmethod
    def class_method(cls) -> str:
        return cls.__name__


class Beta(Alpha):
    """Subclass adding extra methods."""

    def method_c(self) -> int:
        return self.x * 2

    def method_d(self, y: int, z: int) -> int:
        return self.x + y + z

    @staticmethod
    def static_method_beta() -> str:
        return "beta.static"


class Gamma(Beta):
    """Deeper subclass."""

    def method_e(self) -> int:
        return self.x - 1

    def method_f(self) -> int:
        return self.x + 100


class Delta:
    """Disjoint class with its own methods."""

    def method_g(self) -> int:
        return 7

    def method_h(self) -> int:
        return 8

    def method_i(self) -> int:
        return 9


class Epsilon:
    """Yet another class to push class count over 5."""

    def method_j(self) -> str:
        return "epsilon.j"

    def method_k(self) -> str:
        return "epsilon.k"


class Zeta:
    """Class with a nested class."""

    class Inner:
        def inner_method(self) -> int:
            return 0

    def method_l(self) -> int:
        return 1


@simple_decorator
class DecoratedClassOne:
    def method_m(self) -> int:
        return 1


@logging_decorator
class DecoratedClassTwo:
    def method_n(self) -> int:
        return 2


# ----- module-level private helpers -----------------------------------------

def _private_alpha() -> int:
    return 1


def _private_beta() -> int:
    return 2


def _private_gamma(x: int) -> int:
    return x


# ----- filler: additional top-level functions to push LOC ------------------

def filler_one() -> int:
    return 1


def filler_two() -> int:
    return 2


def filler_three() -> int:
    return 3


def filler_four() -> int:
    return 4


def filler_five() -> int:
    return 5


def filler_six() -> int:
    return 6


def filler_seven() -> int:
    return 7


def filler_eight() -> int:
    return 8


def filler_nine() -> int:
    return 9


def filler_ten() -> int:
    return 10


def filler_eleven() -> int:
    return 11


def filler_twelve() -> int:
    return 12


def filler_thirteen() -> int:
    return 13


def filler_fourteen() -> int:
    return 14


def filler_fifteen() -> int:
    return 15


def filler_sixteen() -> int:
    return 16


def filler_seventeen() -> int:
    return 17


def filler_eighteen() -> int:
    return 18


def filler_nineteen() -> int:
    return 19


def filler_twenty() -> int:
    return 20


# ----- filler classes -------------------------------------------------------

class FillerClassOne:
    def f_one_method_one(self) -> int:
        return 1

    def f_one_method_two(self) -> int:
        return 2


class FillerClassTwo:
    def f_two_method_one(self) -> int:
        return 1

    def f_two_method_two(self) -> int:
        return 2


class FillerClassThree:
    def f_three_method_one(self) -> int:
        return 1

    def f_three_method_two(self) -> int:
        return 2


class FillerClassFour:
    def f_four_method_one(self) -> int:
        return 1

    def f_four_method_two(self) -> int:
        return 2


class FillerClassFive:
    def f_five_method_one(self) -> int:
        return 1

    def f_five_method_two(self) -> int:
        return 2


# ----- __main__ trailer (no emitted symbols) --------------------------------

if __name__ == "__main__":
    instance = Alpha(1)
    print(instance.method_a())
    print(top_alpha(2, 3))
    print(top_beta())
    print(top_gamma([1, 2, 3]))
    print(top_delta("x"))
    print(top_epsilon(5, 2))
    print(top_zeta(1, 2, 3))
    print(top_eta(a=1, b=2))
    print(top_theta(lambda n: n * 2, 5))
    print(top_iota([10, 20]))
    print(top_kappa("/tmp"))
    print(decorated_one())
    print(decorated_two())
    print(decorated_three(3))
    print(decorated_four())
    print(Beta(2).method_c())
    print(Gamma(3).method_e())
    print(Delta().method_g())
    print(Epsilon().method_j())
    print(Zeta().method_l())
    print(Zeta.Inner().inner_method())
    print(DecoratedClassOne().method_m())
    print(DecoratedClassTwo().method_n())
    print(_private_alpha())
    print(_private_beta())
    print(_private_gamma(9))
    print(filler_one(), filler_two(), filler_three(), filler_four(), filler_five())
    print(filler_six(), filler_seven(), filler_eight(), filler_nine(), filler_ten())
    print(filler_eleven(), filler_twelve(), filler_thirteen(), filler_fourteen(), filler_fifteen())
    print(filler_sixteen(), filler_seventeen(), filler_eighteen(), filler_nineteen(), filler_twenty())
    print(FillerClassOne().f_one_method_one())
    print(FillerClassTwo().f_two_method_one())
    print(FillerClassThree().f_three_method_one())
    print(FillerClassFour().f_four_method_one())
    print(FillerClassFive().f_five_method_one())
