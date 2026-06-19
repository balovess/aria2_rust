# aria2-rust Testing Guide

This document provides comprehensive guidance on testing practices for the aria2-rust project.

## Table of Contents

- [Testing Best Practices](#testing-best-practices)
- [Test Naming Conventions](#test-naming-conventions)
- [Test Categories](#test-categories)
- [Test Helpers and Utilities](#test-helpers-and-utilities)
- [Writing Different Test Types](#writing-different-test-types)
- [Coverage Requirements](#coverage-requirements)
- [Running Tests](#running-tests)
- [Debugging Tests](#debugging-tests)

---

## Testing Best Practices

### General Principles

1. **Isolation**: Each test should be independent and not rely on other tests' state.
2. **Determinism**: Tests must produce the same results every time they run.
3. **Speed**: Unit tests should be fast; integration tests may take longer but should be optimized.
4. **Clarity**: Test names and assertions should clearly express what is being tested and why.
5. **Coverage**: Aim for comprehensive coverage of all critical paths, edge cases, and error scenarios.

### Rust Testing Conventions

```rust
// Use #[test] for synchronous tests
#[test]
fn test_example() {
    assert_eq!(2 + 2, 4);
}

// Use #[tokio::test] for async tests
#[tokio::test]
async fn test_async_example() {
    let result = some_async_function().await;
    assert!(result.is_ok());
}

// Use multi-threaded runtime for stress tests
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_concurrent() {
    // ...
}
```

### Test Organization

The project follows a layered testing approach:

| Layer | Location | Purpose |
|-------|----------|---------|
| Unit Tests | Inline in `src/**/*.rs` under `#[cfg(test)]` modules | Test individual functions/methods |
| Integration Tests | `tests/` directory in each crate | Test module interactions |
| E2E Tests | `tests/test_e2e_*.rs` files | Test full download workflows |
| Stress Tests | `tests/test_stress_*.rs` files | Test under high load |
| Edge Case Tests | `tests/test_edge_*.rs` files | Test boundary conditions |
| Error Path Tests | `tests/test_error_*.rs` files | Test error handling |
| Benchmarks | `benches/*.rs` files | Performance testing |

---

## Test Naming Conventions

All tests must follow the naming pattern: `test_<module>_<feature>_<scenario>`

### Naming Pattern Breakdown

- **module**: The module or component being tested (e.g., `uri`, `download`, `config`)
- **feature**: The specific functionality being tested (e.g., `validation`, `creation`, `execution`)
- **scenario**: The specific test scenario (e.g., `empty_input`, `success`, `timeout`, `invalid_format`)

### Examples

```rust
// Good naming examples
#[test]
fn test_uri_validation_empty_input() { ... }

#[test]
fn test_download_command_creation_success() { ... }

#[tokio::test]
async fn test_http_connection_timeout_slow_server() { ... }

#[test]
fn test_config_parser_invalid_file() { ... }

#[test]
fn test_bittorrent_piece_selector_rarest_first() { ... }

// Stress test naming
#[tokio::test]
async fn test_stress_100_concurrent_downloads() { ... }

// Edge case naming
#[test]
fn test_edge_empty_uri_validation() { ... }

// Error path naming
#[tokio::test]
async fn test_error_network_connection_reset() { ... }
```

### Naming for Different Test Categories

| Category | Prefix | Example |
|----------|--------|---------|
| Unit | `test_` | `test_uri_parse_valid_http()` |
| Integration | `test_` | `test_download_engine_http_flow()` |
| E2E | `test_e2e_` | `test_e2e_http_download_complete()` |
| Stress | `test_stress_` | `test_stress_concurrent_downloads()` |
| Edge Case | `test_edge_` | `test_edge_empty_input()` |
| Error Path | `test_error_` | `test_error_network_timeout()` |
| Regression | `test_` (in `regression/` dir) | `test_cli_options_compat()` |
| Performance | `bench_` (in benches) | `bench_config_parser()` |

---

## Test Categories

### 1. Unit Tests

Unit tests are written inline in source files under `#[cfg(test)] mod tests` blocks.

**Location**: `src/**/*.rs` (inline)

**Purpose**: Test individual functions, methods, and small components.

**Example**:
```rust
// src/validation/uri.rs

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uri_validation_valid_http() {
        let result = validate("http://example.com/file.zip");
        assert!(result.is_ok());
    }

    #[test]
    fn test_uri_validation_empty_input() {
        let result = validate("");
        assert!(result.is_err());
    }
}
```

### 2. Integration Tests

Integration tests verify interactions between multiple modules.

**Location**: `tests/` directory in each crate

**Purpose**: Test module boundaries and interactions.

**Key Files**:
- `aria2-core/tests/engine_integration_tests.rs` - Engine integration
- `aria2-core/tests/ftp_integration_test.rs` - FTP protocol integration
- `aria2-core/tests/dht_integration_tests.rs` - DHT integration
- `aria2-rpc/tests/integration_rpc.rs` - RPC integration

### 3. E2E Tests

End-to-end tests simulate complete user workflows.

**Location**: `tests/test_e2e_*.rs` files

**Purpose**: Test complete download workflows from start to finish.

**Key Files**:
- `test_e2e_download.rs` - HTTP download E2E
- `test_e2e_bittorrent_download.rs` - BitTorrent E2E
- `test_e2e_ftp_download.rs` - FTP E2E
- `test_e2e_magnet_download.rs` - Magnet link E2E
- `test_e2e_metalink_download.rs` - Metalink E2E
- `test_e2e_concurrent_download.rs` - Concurrent downloads E2E

### 4. Stress Tests

Stress tests verify system stability under high load.

**Location**: `tests/test_stress_*.rs` files

**Purpose**: Verify no deadlocks, panics, or resource leaks under load.

**Key Files**:
- `test_stress_concurrent_downloads.rs` - 100+ concurrent downloads
- `test_stress_rpc_concurrent.rs` - RPC under load

**Example Pattern**:
```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_100_concurrent_downloads() {
    let server = MockHttpServer::start().await.unwrap();
    
    // Track memory before
    let mem_before = get_memory_usage();
    
    // Spawn 100 concurrent tasks
    let handles = (0..100).map(|i| {
        tokio::spawn(async move { /* download task */ })
    });
    
    let results = futures::future::join_all(handles).await;
    
    // Verify all completed
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 100);
    
    // Verify memory bounded
    let mem_after = get_memory_usage();
    assert!(mem_after - mem_before < 50_000_000);
}
```

### 5. Edge Case Tests

Edge case tests handle boundary conditions and unusual inputs.

**Location**: `tests/test_edge_*.rs` files

**Purpose**: Verify graceful handling of edge cases without panics.

**Key Files**:
- `test_edge_empty_input.rs` - Empty/whitespace inputs
- `test_edge_invalid_input.rs` - Invalid format inputs

**Example Pattern**:
```rust
#[test]
fn test_edge_empty_uri_validation() {
    let result = validate("");
    assert!(result.is_err(), "Empty URI should return error");
}

#[test]
fn test_edge_very_long_whitespace_uri() {
    let long_whitespace = " ".repeat(10000);
    let result = validate(&long_whitespace);
    assert!(result.is_err());
}

#[test]
fn test_edge_uri_with_null_byte() {
    let result = validate("http://example.com/file\0.txt");
    // Should handle gracefully - no panic
    let _ = result;
}
```

### 6. Error Path Tests

Error path tests verify proper error handling and recovery.

**Location**: `tests/test_error_*.rs` files

**Purpose**: Test error scenarios, retry logic, and recovery.

**Key Files**:
- `test_error_network.rs` - Network errors (timeout, connection reset)
- `test_error_disk.rs` - Disk errors (space exhausted, permission denied)

**Example Pattern**:
```rust
#[tokio::test]
async fn test_error_network_connection_timeout() {
    let server = MockHttpServer::start().await.unwrap();
    server.register_slow_response("/slow.bin", 500, &data);
    
    let config = HttpConfig {
        connect_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    
    let result = manager.acquire(&url).await;
    
    match result {
        Err(Aria2Error::Recoverable(RecoverableError::Timeout)) => {
            // Expected behavior
        }
        Err(e) => {
            assert!(e.to_string().contains("timeout"));
        }
        Ok(_) => { /* may succeed if TCP completes */ }
    }
}
```

---

## Test Helpers and Utilities

### Core Test Helpers

The project provides shared test utilities in multiple locations:

#### 1. `aria2-core/tests/test_harness.rs`

Core test utilities for E2E tests:

```rust
use test_harness::*;

// Create auto-cleaning temp directory
let dir = setup_temp_dir();

// Assert file contents
assert_file_contents(&path, &expected_bytes);

// Assert SHA256 checksum
assert_file_sha256(&path, "abc123...");

// Wait for condition with timeout
let result = wait_for(5, || {
    if path.exists() { Some(true) } else { None }
}).await;

// Generate deterministic test data
let data = generate_test_data(1024, 0x42);
```

#### 2. `aria2/tests/helpers/mod.rs`

Integration test helpers:

```rust
use helpers::*;

// Wait for file creation
wait_for_file(&path, Duration::from_secs(5));

// Wait for download completion
wait_for_download_complete(&output_path, 1024, 10);

// Assert file content
assert_file_content(&path, &expected);

// Get binary path for CLI tests
let binary = get_binary_path();

// Generate test data
let data = generate_test_data(100, 0x42);
```

#### 3. `aria2-core/tests/fixtures/mod.rs`

Mock server fixtures:

```rust
use fixtures::*;

// Mock HTTP server
let server = test_server::TestServer::start();

// Mock BitTorrent peer
let peer = mock_bt_peer::MockBtPeer::start();

// Mock DHT node
let dht = mock_dht_node::MockDhtNode::start();

// Mock tracker
let tracker = mock_tracker::MockTracker::start();

// Torrent builder
let torrent = test_torrent_builder::build_single_file(1024);

// Metalink builder
let metalink = test_metalink_builder::build_simple();
```

#### 4. `aria2-core/tests/e2e_helpers/mod.rs`

E2E test utilities:

```rust
use e2e_helpers::*;

// Mock HTTP server with range support
let server = mock_http_server::MockHttpServer::start().await.unwrap();
server.register_range_response("/file.bin", &data);
server.register_partial_serve("/partial.bin", &full_data, partial_len);
server.register_slow_response("/slow.bin", delay_ms, &data);

// Mock torrent helper
let torrent = mock_torrent::create_test_torrent(files);
```

### Using Test Helpers

```rust
// Example: Complete E2E test using helpers
mod e2e_helpers;
use e2e_helpers::mock_http_server::MockHttpServer;

#[tokio::test]
async fn test_e2e_http_download_complete() {
    // Setup
    let server = MockHttpServer::start().await.expect("Server start failed");
    let test_data = generate_test_data(1024, 0x42);
    server.register_range_response("/test.bin", &test_data);
    
    let dir = setup_temp_dir();
    let url = format!("{}/test.bin", server.base_url());
    
    // Execute
    let mut cmd = DownloadCommand::new(
        GroupId::new(1),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    ).expect("Command creation failed");
    
    let result = cmd.execute().await;
    
    // Verify
    assert!(result.is_ok());
    assert_file_contents(&dir.path().join("test.bin"), &test_data);
    
    // Cleanup
    server.shutdown().await;
}
```

---

## Writing Different Test Types

### Writing Unit Tests

Unit tests should be placed inline in source files:

```rust
// src/config/parser.rs

pub fn parse_option(line: &str) -> Result<(String, OptionValue), ParseError> {
    // Implementation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parser_parse_option_valid_string() {
        let result = parse_option("dir=/downloads");
        assert!(result.is_ok());
        let (key, value) = result.unwrap();
        assert_eq!(key, "dir");
        assert_eq!(value, OptionValue::Str("/downloads".into()));
    }

    #[test]
    fn test_parser_parse_option_empty_line() {
        let result = parse_option("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parser_parse_option_whitespace_only() {
        let result = parse_option("   ");
        assert!(result.is_err());
    }
}
```

### Writing Integration Tests

Integration tests go in the `tests/` directory:

```rust
// tests/engine_integration_tests.rs

use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::request::request_group_man::RequestGroupMan;

#[tokio::test]
async fn test_engine_integration_add_and_remove_group() {
    let engine = DownloadEngine::new(10);
    let manager = RequestGroupMan::new();
    
    // Add group
    let gid = manager.add_group(
        vec!["http://example.com/file.bin"],
        Default::default(),
    ).await.expect("Add group failed");
    
    // Verify group exists
    assert_eq!(manager.count().await, 1);
    
    // Remove group
    manager.remove_group(gid).await.expect("Remove failed");
    
    // Verify empty
    assert_eq!(manager.count().await, 0);
}
```

### Writing Stress Tests

Stress tests verify stability under high load:

```rust
// tests/test_stress_concurrent_downloads.rs

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_100_concurrent_downloads() {
    let server = MockHttpServer::start().await.unwrap();
    server.register_range_response("/file.bin", &[0xABu8; 1024]);
    
    let manager = Arc::new(RequestGroupMan::new());
    let semaphore = Arc::new(Semaphore::new(50));
    
    // Spawn 100 tasks
    let handles = (0..100).map(|i| {
        let m = manager.clone();
        let s = semaphore.clone();
        tokio::spawn(async move {
            let _permit = s.acquire().await.unwrap();
            m.add_group(vec![url], Default::default()).await
        })
    }).collect::<Vec<_>>();
    
    let results = futures::future::join_all(handles).await;
    
    // Verify all completed without panic
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 100);
    
    // Verify memory bounded
    let mem_growth = get_memory_usage() - mem_before;
    assert!(mem_growth < 50_000_000, "Memory should not grow unboundedly");
}
```

### Writing Edge Case Tests

Edge case tests handle boundary conditions:

```rust
// tests/test_edge_empty_input.rs

#[test]
fn test_edge_empty_uri_validation() {
    let result = validate("");
    assert!(result.is_err(), "Empty URI should return error");
}

#[test]
fn test_edge_whitespace_only_uri() {
    let result = validate("   ");
    assert!(result.is_err());
}

#[test]
fn test_edge_very_long_whitespace() {
    let input = " ".repeat(10000);
    let result = validate(&input);
    assert!(result.is_err());
}

#[test]
fn test_edge_uri_with_null_byte() {
    let result = validate("http://example.com/file\0.txt");
    // Should not panic
    let _ = result;
}

#[test]
fn test_edge_empty_torrent_file() {
    let empty: Vec<u8> = Vec::new();
    let result = BtDownloadCommand::new(GroupId::new(1), &empty, &Default::default(), None);
    assert!(result.is_err());
}
```

### Writing Error Path Tests

Error path tests verify error handling:

```rust
// tests/test_error_network.rs

#[tokio::test]
async fn test_error_network_connection_timeout() {
    let server = MockHttpServer::start().await.unwrap();
    server.register_slow_response("/slow.bin", 500, &data);
    
    let config = HttpConfig {
        connect_timeout: Duration::from_millis(100),
        ..Default::default()
    };
    
    let result = manager.acquire(&url).await;
    
    assert!(result.is_err());
    match result.unwrap_err() {
        Aria2Error::Recoverable(RecoverableError::Timeout) => {},
        e => assert!(e.to_string().contains("timeout")),
    }
}

#[tokio::test]
async fn test_error_network_retry_recovery() {
    let policy = RetryPolicy::new(3, 10);
    let stats = RetryStats::default();
    let executor = RetryExecutor::new(&policy, &stats);
    
    let result = executor.execute(|attempt| async move {
        if attempt < 2 {
            Err(Aria2Error::Recoverable(RecoverableError::Timeout))
        } else {
            Ok("success")
        }
    }).await;
    
    assert!(result.is_ok());
    assert_eq!(stats.timeouts(), 2);
}

#[test]
fn test_error_disk_space_exhausted() {
    // Simulate disk space check
    let result = check_disk_space("/nonexistent", 1_000_000_000);
    assert!(result.is_err());
}
```

---

## Coverage Requirements

### Minimum Coverage Targets

| Component | Minimum Coverage | Notes |
|-----------|------------------|-------|
| Core Engine | 80% | Critical download logic |
| Protocol Handlers | 75% | HTTP, FTP, BitTorrent |
| RPC Server | 70% | API compatibility |
| Configuration | 85% | Option parsing and validation |
| Error Handling | 90% | All error paths must be tested |
| Utility Functions | 60% | Helper functions |

### Coverage Categories

1. **Line Coverage**: Percentage of code lines executed
2. **Branch Coverage**: Percentage of conditional branches executed
3. **Function Coverage**: Percentage of functions called

### Generating Coverage Reports

```bash
# Install cargo-tarpaulin (Linux/macOS)
cargo install cargo-tarpaulin

# Generate coverage report
cargo tarpaulin --workspace --out Html --output-dir coverage/

# Generate coverage with specific features
cargo tarpaulin --workspace --features bittorrent --out Html

# Generate LCOV format for CI
cargo tarpaulin --workspace --out Lcov --output-dir coverage/
```

### Coverage Best Practices

1. **Test All Paths**: Ensure both success and error paths are covered
2. **Test Edge Cases**: Boundary conditions often reveal bugs
3. **Test Error Recovery**: Retry logic and fallback mechanisms
4. **Test Concurrent Code**: Race conditions and deadlocks
5. **Avoid Dead Code**: Remove unused code to improve coverage metrics

---

## Running Tests

### Basic Test Commands

```bash
# Run all tests in workspace
cargo test --workspace

# Run tests for specific crate
cargo test -p aria2-core

# Run tests with verbose output
cargo test --workspace -- --nocapture

# Run specific test
cargo test test_uri_validation_empty_input

# Run tests matching pattern
cargo test "test_e2e"

# Run ignored tests
cargo test --workspace -- --ignored
```

### Running Specific Test Categories

```bash
# Run E2E tests only
cargo test "test_e2e"

# Run stress tests only
cargo test "test_stress"

# Run edge case tests only
cargo test "test_edge"

# Run error path tests only
cargo test "test_error"

# Run integration tests
cargo test --test integration_tests
```

### Running Benchmarks

```bash
# Run all benchmarks
cargo bench --workspace

# Run specific benchmark
cargo bench --bench config_bench

# Run benchmark with specific parameters
cargo bench -- --sample-size 100
```

### Test Environment Setup

Some tests require specific environment setup:

```bash
# Set test timeout
export TEST_TIMEOUT=30

# Enable debug logging for tests
export RUST_LOG=debug

# Use test configuration
export ARIA2_CONF_PATH=tests/fixtures/test.conf
```

---

## Debugging Tests

### Common Debugging Techniques

```rust
// Use println! for debug output (requires --nocapture)
#[test]
fn test_with_debug_output() {
    println!("Debug: value = {}", value);
    assert!(condition);
}

// Use dbg! for quick debugging
#[test]
fn test_with_dbg() {
    dbg!(&result);
    assert!(result.is_ok());
}

// Use tracing for structured logging
#[test]
fn test_with_tracing() {
    tracing::info!("Starting test");
    tracing::debug!("Intermediate value: {}", value);
}
```

### Running Tests with Debug Output

```bash
# Show test output
cargo test -- --nocapture

# Show test output with specific test
cargo test test_specific -- --nocapture

# Run single test with full output
cargo test test_specific --exact -- --nocapture
```

### Debugging Async Tests

```rust
#[tokio::test]
async fn test_async_debug() {
    // Add timeout to prevent hanging
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        some_async_operation()
    ).await;
    
    match result {
        Ok(inner) => println!("Completed: {:?}", inner),
        Err(_) => println!("Timeout - test may be stuck"),
    }
}
```

### Common Test Issues

1. **Test Hanging**: Add timeouts to async tests
2. **Race Conditions**: Use synchronization primitives (Mutex, RwLock)
3. **Resource Leaks**: Ensure cleanup in test teardown
4. **Flaky Tests**: Make tests deterministic with fixed seeds/inputs
5. **Memory Issues**: Check for unbounded growth in stress tests

---

## Appendix: Test File Reference

### Test Files by Category

| File | Category | Description |
|------|----------|-------------|
| `test_harness.rs` | Helpers | Core test utilities |
| `test_e2e_download.rs` | E2E | HTTP download workflow |
| `test_e2e_bittorrent_download.rs` | E2E | BitTorrent workflow |
| `test_e2e_ftp_download.rs` | E2E | FTP workflow |
| `test_e2e_magnet_download.rs` | E2E | Magnet link workflow |
| `test_e2e_metalink_download.rs` | E2E | Metalink workflow |
| `test_e2e_concurrent_download.rs` | E2E | Concurrent downloads |
| `test_stress_concurrent_downloads.rs` | Stress | High concurrency |
| `test_stress_rpc_concurrent.rs` | Stress | RPC under load |
| `test_edge_empty_input.rs` | Edge | Empty inputs |
| `test_edge_invalid_input.rs` | Edge | Invalid formats |
| `test_error_network.rs` | Error | Network errors |
| `test_error_disk.rs` | Error | Disk errors |
| `engine_integration_tests.rs` | Integration | Engine integration |
| `ftp_integration_test.rs` | Integration | FTP integration |
| `dht_integration_tests.rs` | Integration | DHT integration |

### Benchmark Files

| File | Description |
|------|-------------|
| `config_bench.rs` | Configuration parsing performance |
| `engine_bench.rs` | Download engine performance |
| `filesystem_bench.rs` | Disk I/O performance |
| `p2_bench.rs` | P2 protocol performance |
| `serialization_bench.rs` | Serialization performance |
| `rpc_bench.rs` | RPC handling performance |

---

## Contributing Tests

When adding new features, ensure:

1. Add unit tests inline in source files
2. Add integration tests for module interactions
3. Add E2E tests for complete workflows
4. Add edge case tests for boundary conditions
5. Add error path tests for failure scenarios
6. Update coverage reports

### Test PR Checklist

- [ ] Tests follow naming convention `test_<module>_<feature>_<scenario>`
- [ ] All tests pass locally
- [ ] No test hangs or timeouts
- [ ] Proper cleanup in test teardown
- [ ] Coverage meets minimum requirements
- [ ] Stress tests verify memory bounds
- [ ] Edge cases handled without panic