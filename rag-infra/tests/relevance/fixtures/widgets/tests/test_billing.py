from billing.invoice import Invoice


def test_invoice_total_rounds_half_even():
    invoice = Invoice("INV-1", [("A-1", 3, "0.125")])
    assert str(invoice.total()) == "0.38"


def test_invoice_total_of_no_items_is_zero():
    assert str(Invoice("INV-2", []).total()) == "0.00"
