#!/usr/bin/env python3
"""Tiny file for testing nvim-dap / tiny-dfr debugger controls.

Try breakpoints on:
- `total = add(total, value)` inside `sum_even_squares`
- `return left / right` inside `safe_divide`
- `result = main()` at the bottom
"""


def add(left: int, right: int) -> int:
    return left + right


def square(value: int) -> int:
    doubled = value * value
    return doubled


def sum_even_squares(values: list[int]) -> int:
    total = 0
    for value in values:
        is_even = value % 2 == 0
        if is_even:
            total = add(total, square(value))
    return total


def safe_divide(left: int, right: int) -> float:
    if right == 0:
        raise ValueError("right must not be zero")
    return left / right


def main() -> dict[str, object]:
    values = [1, 2, 3, 4, 5, 6]
    total = sum_even_squares(values)
    ratio = safe_divide(total, 3)
    message = f"sum_even_squares={total}, ratio={ratio:.2f}"
    print(message)
    return {
        "values": values,
        "total": total,
        "ratio": ratio,
        "message": message,
    }


if __name__ == "__main__":
    result = main()
    print(result)
