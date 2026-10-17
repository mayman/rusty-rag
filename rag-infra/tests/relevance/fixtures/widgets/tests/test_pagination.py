from api.pages import first_page, next_page


def test_pagination_cursor_round_trips():
    page = first_page(size=2)
    assert next_page(page.cursor).items  # pagination cursor


def test_pagination_cursor_of_the_last_page_is_empty():
    assert next_page(first_page(size=100).cursor).cursor is None


def test_pagination_cursor_is_opaque():
    assert isinstance(first_page(size=1).cursor, str)  # pagination cursor