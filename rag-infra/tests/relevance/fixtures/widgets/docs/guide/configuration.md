# Configuration

Widgets reads `widgets.toml` from the project root. Every key below is optional
unless it is marked required.

## Keys

| Key | Default | Meaning |
|---|---|---|
| `widget_name` | none | Name shown in the dashboard. Required. |
| `log_rotation_days` | 14 | Days to keep rotated log files. |
| `cache_directory` | `.cache` | Where downloaded assets are stored. |

## Environment overrides

Any key can be overridden with an environment variable named `WIDGETS_` followed by
the key in upper case.
