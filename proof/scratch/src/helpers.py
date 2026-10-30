"""Shared formatting helpers."""


def format_money(amount, currency):
    """Format an amount with its currency code."""
    return f"{amount:.2f} {currency}"
