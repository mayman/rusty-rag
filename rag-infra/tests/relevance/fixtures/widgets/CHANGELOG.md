# Changelog

All notable changes to widgets are documented here.

## [2.3.0] - 2026-06-01

### Added

- `--offset` paging for `widgets search`.
- A new setting for the cache location in the configuration file.
- Widget dashboards show the session owner in the header.

## [2.2.1] - 2026-05-14

### Fixed

- Fixed retry backoff overflow after roughly thirty attempts; the delay now saturates.
- Widget thumbnails no longer render twice after a reload.

## [2.2.0] - 2026-04-02

### Changed

- Widget dashboards render through the new router.
- Widget thumbnails are cached per widget name.
