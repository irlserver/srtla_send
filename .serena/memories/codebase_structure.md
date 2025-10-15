# Codebase Structure

## Directory Layout
```
src/
  tests/
    connection_tests.rs
    end_to_end_tests.rs
    integration_tests.rs
    mod.rs
    protocol_tests.rs
    registration_tests.rs
    sender_tests.rs
    toggles_tests.rs
  connection.rs       - Connection management and quality scoring
  lib.rs             - Library exports
  main.rs            - CLI entry point
  protocol.rs        - SRTLA protocol definitions
  registration.rs    - Registration flow (REG1/REG2/REG3)
  sender.rs          - Main sender logic and packet forwarding
  test_helpers.rs    - Test utilities
  toggles.rs         - Runtime toggle system
  utils.rs           - Utility functions
.cargo/
  config.toml
.github/
  workflows/
    build-debian.yml
    ci.yml
Cargo.toml          - Project manifest
build.rs            - Build script
rustfmt.toml        - Formatting configuration
```

## Module Organization
- `connection`: SrtlaConnection struct, bind/resolve utilities, incoming packet handling
- `protocol`: SRTLA protocol constants and structures
- `registration`: Registration manager for SRTLA connection setup
- `sender`: Main packet forwarding logic, connection selection algorithm
- `toggles`: Runtime toggle system for classic mode, quality, exploration
- `utils`: Common utilities (now_ms, etc.)

## Test Organization
Tests are located in `src/tests/`:
- Unit tests: In-module tests for individual components
- Integration tests: Cross-module tests
- End-to-end tests: Full system tests
- Protocol tests: SRTLA protocol implementation tests
- Feature flag: `test-internals` exposes internal fields for testing
