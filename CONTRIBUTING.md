# Contributing

This project is licensed under GPL-3.0-only.

## Development

Run checks before proposing changes:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Package check:

```bash
cargo package --no-verify
```

Install locally:

```bash
cargo install --path .
```

## Versioning

Every code change must bump the package version in both `Cargo.toml` and `Cargo.lock`.

## Naming

- Product and repository: `locator`
- Command: `lctr`
- Local index directory: `.locator`
- Environment variables: `LCTR_DB`, `LCTR_DATA_DIR`

Do not add new user-facing references to the old private name.
