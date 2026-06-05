# Changelog - v0.1.60

- Added automatic GitHub release check: `lctr` now notifies you when a newer version is available whenever you run `scan` or `search`.
- The update notice appears as a banner in the search TUI, and as a line after the scan summary.
- The check is cached for 24 hours so it never slows down repeated launches.
- Opt out with `--no-update-check` (persists across runs) or `LCTR_NO_UPDATE_CHECK=1`.
