set ignore-comments
  
# Hidden default lists all available recipes.
_default:
  @just --list --list-heading $'KYOTO\n'
  
# Quick check of the code including lints and formatting.
check:
  cargo fmt -- --check
  # Turn warnings into errors.
  cargo clippy --all-targets -- -D warnings
  cargo check --all-features

# Run a test suite: unit, integration, features, msrv, min-versions, or sync.
test suite="unit":
  just _test-{{suite}}

# Unit test suite.
_test-unit:
  cargo test --lib
  cargo test --doc
  cargo test --examples

# Run integration tests, excluding the network sync.
_test-integration: 
  cargo test --tests -- --test-threads 1 --nocapture

# Run the network sync example.
_test-sync: 
  cargo run --example bitcoin --release

# Test feature flag matrix compatibility.
_test-features:
  # Build and test with all features, no features, and some combinations.
  cargo test --lib --all-features
  cargo test --lib --no-default-features

# Test that minimum versions of dependency constraints are still valid.
_test-min-versions:
  just _delete-lockfile
  cargo +nightly check --all-features -Z direct-minimal-versions

# Check code with MSRV compiler.
_test-msrv:
  # Handles creating sandboxed environments to ensure no newer binaries sneak in.
  cargo install cargo-msrv@0.18.4
  cargo msrv verify --all-features

# Run the example: signet, testnet, or bitcoin.
example name="signet":
  cargo run --example {{name}} --release

# Delete unused files or branches: lockfile, branches
delete item="branches":
  just _delete-{{item}}

_delete-lockfile:
  rm -f Cargo.lock

_delete-branches:
  git branch --merged | grep -v \* | xargs git branch -d
