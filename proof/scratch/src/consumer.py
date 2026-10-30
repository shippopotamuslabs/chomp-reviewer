"""Order consumer: renders fetched orders for the dashboard."""

from src.helpers import format_money
from src.store import fetch_order


def order_summary(order_id):
    """Render one line for the dashboard."""
    order = fetch_order(order_id)
    return f"order {order['id']}: {format_money(order['total'], order['currency'])}"
