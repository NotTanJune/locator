# Changelog - v0.1.58

- Scan progress now uses a cleaner inline dashboard with borders, color, progress, rate, health, and current-path sections.
- Completed scans now show a dashboard-style summary with timing bars, next commands, and compact error details.
- The existing scan animation is unchanged, but it now sits inside the structured scan dashboard.
- Added a small terminal `locator` wordmark to scan dashboard headers.
- Release note drafts now use the `Changelog - vX` title format.
- Install paths now refresh package-manager metadata or Rust toolchains before installing.
- `cargo install --path .` and Cargo fallback installs now resolve newer allowed dependencies instead of forcing the lockfile.
- Scan completion now hides staging implementation paths and focuses the dashboard on indexing speed, files indexed, next commands, and clearer scan breakdowns.
- Scan completion diagnostics now use clearer labels for post-scan index building and filesystem scan counts.
- Non-blocking scan issues now appear as yellow warnings instead of red errors.
- Scan warning details now use the full dashboard width with wrapped sample paths.
