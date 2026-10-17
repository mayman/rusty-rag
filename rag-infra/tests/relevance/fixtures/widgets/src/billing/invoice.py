"""Invoices and their line items."""

from decimal import ROUND_HALF_EVEN, Decimal


class InvoiceLineItem:
    def __init__(self, sku, quantity, unit_price):
        self.sku = sku
        self.quantity = quantity
        self.unit_price = Decimal(unit_price)

    def subtotal(self):
        amount = self.unit_price * self.quantity
        return amount.quantize(Decimal("0.01"), rounding=ROUND_HALF_EVEN)


class Invoice:
    def __init__(self, number, line_items):
        self.number = number
        self.line_items = [InvoiceLineItem(*raw) for raw in line_items]

    def total(self):
        return sum((item.subtotal() for item in self.line_items), Decimal("0.00"))
