# Suggested Commands

## Development Commands

### Building
```bash
# Standard build
cargo build

# Release build
cargo build --release

# Release with debug symbols
cargo build --profile release-debug

# Release with fat LTO (optimized)
cargo build --profile release-lto
```

### Testing
```bash
# Run all tests (requires nightly, with test-internals)
cargo test --features test-internals

# Run with verbose output
cargo test --features test-internals --verbose

# Run all tests with all features
cargo test --all-features --verbose

# Run only unit tests (library tests)
cargo test --lib --verbose

# Run specific test
cargo test test_connection_score

# Run unit tests without test features (verify encapsulation)
cargo test --lib
```

### Formatting
```bash
# Format code (requires nightly)
cargo fmt --all

# Check formatting without modifying
cargo fmt --all -- --check
```

### Linting
```bash
# Run clippy (warnings as errors)
cargo clippy -- -D warnings

# Check compilation
cargo check

# Check release compilation
cargo check --release
```

### Security
```bash
# Install cargo-audit (first time only)
cargo install cargo-audit

# Run security audit
cargo audit
```

### Running
```bash
# Run with logging
RUST_LOG=info cargo run -- 6000 rec.example.com 5000 ./uplinks.txt

# Run with debug logging
RUST_LOG=debug cargo run -- 6000 rec.example.com 5000 ./uplinks.txt

# Run binary directly (after build)
./target/release/srtla_send 6000 rec.example.com 5000 ./uplinks.txt
```

## System Commands (Linux/WSL)
- `ls` - List files
- `cd` - Change directory
- `grep` / `rg` - Search text (prefer rg/ripgrep)
- `find` - Find files
- `cat` - View file contents
- `git` - Version control
- `kill -HUP <pid>` - Reload IP list (Unix only)
