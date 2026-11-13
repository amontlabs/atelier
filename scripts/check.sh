#!/bin/sh
# Checks atelier until atelier can check itself: format, lints, every test.
set -eu
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
