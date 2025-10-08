# Task Completion Checklist

When a coding task is completed, the following steps MUST be performed:

## 1. Format Code
```bash
cargo fmt --all
```
Verify it passes with:
```bash
cargo fmt --all -- --check
```

## 2. Run Clippy
```bash
cargo clippy -- -D warnings
```
All clippy warnings must be resolved (treated as errors).

## 3. Check Compilation
```bash
cargo check
cargo check --release
```

## 4. Run Tests
```bash
# Run all tests with test-internals feature
cargo test --features test-internals --verbose

# Optionally run without test features to verify encapsulation
cargo test --lib --verbose
```

## 5. Build Verification
```bash
cargo build --release
```

## Important Notes
- **NEVER commit changes unless explicitly asked by the user**
- All steps must pass before considering the task complete
- Tests require the `test-internals` feature for full coverage
- The project requires Rust nightly toolchain
- Use `RUST_LOG=info` or `RUST_LOG=debug` for runtime debugging
