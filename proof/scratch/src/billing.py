"""Billing: invoices rendered from fetched orders."""

from src.helpers import format_money
from src.store import fetch_order


def invoice_line(order_id):
    """Render one invoice line."""
    order = fetch_order(order_id)
    return f"invoice {order['id']}: {format_money(order['total'], order['currency'])}"
