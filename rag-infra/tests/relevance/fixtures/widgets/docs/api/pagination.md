# Pagination

## Pagination cursor

Each page returns a pagination cursor. Pass the cursor back to read the next page.

Cursors are opaque strings: do not parse them, do not store them across releases, and
do not build one by hand. A cursor stays valid while the underlying list is stable,
and the server answers with an error when it has expired, so start again from the
first page in that case.