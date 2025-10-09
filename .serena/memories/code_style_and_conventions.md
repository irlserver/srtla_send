# Code Style and Conventions

## Formatting Configuration (rustfmt.toml)

The project uses **Rust nightly** with unstable rustfmt features:

- Edition: 2024
- `unstable_features = true`
- `wrap_comments = false`
- `imports_granularity = "Module"`
- `group_imports = "StdExternalCrate"` (std, external, crate order)
- `format_code_in_doc_comments = true`
- `format_macro_matchers = true`
- `hex_literal_case = "Lower"`
- `format_strings = true`
- `use_field_init_shorthand = true`
- `use_try_shorthand = true`

## Naming Conventions

- Constants: `SCREAMING_SNAKE_CASE` (e.g., `NAK_SEARCH_LIMIT`, `MIN_SWITCH_INTERVAL_MS`)
- Structs: `PascalCase` (e.g., `SrtlaConnection`, `SequenceTrackingEntry`)
- Functions: `snake_case` (e.g., `handle_srt_packet`, `select_connection_idx`)
- Modules: `snake_case`

## Code Patterns

- **Error Handling**: Uses `anyhow::Result` for most error propagation
- **Visibility**: Uses conditional compilation with `#[cfg(feature = "test-internals")]` to expose internal fields for testing
- **Imports**: Grouped as std → external → crate, with module-level granularity
- **Logging**: Uses `tracing` macros (`debug!`, `info!`, `warn!`, etc.)
- **Async**: Heavy use of Tokio for async networking and timers

## Documentation

- **Comments**: NO COMMENTS unless explicitly asked, but NEVER remove existing comments - update them if needed
- Keep code self-documenting through clear naming
- Use doc comments for public API when necessary

## Important Constraints

- **Requires Rust nightly** due to unstable rustfmt features
- All formatting must pass `cargo fmt --all -- --check`
- All code must pass clippy with `-D warnings` (warnings as errors)
