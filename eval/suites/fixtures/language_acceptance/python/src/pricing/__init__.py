def apply_discount(subtotal: int, percent: int) -> int:
    """Return the discounted subtotal in whole cents."""
    return subtotal + (subtotal * percent // 100)
