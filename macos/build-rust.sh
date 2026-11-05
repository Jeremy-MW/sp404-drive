#!/bin/sh
# Builds the drive's Rust core (crate sp404-ffi) as a static library for the FSKit
# extension. Run by Xcode as a build phase of the SP404FS target (see project.yml); it
# can also be run by hand from anywhere.
#
#   SP404_CARGO_PROFILE  debug (default) or release
#
# Output: target/aarch64-apple-darwin/<profile>/libsp404_ffi.a. Only arm64 for now.
set -eu

# Xcode runs build phases with a minimal PATH: look where rustup puts cargo.
PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH"
export PATH
if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found (looked in /opt/homebrew/opt/rustup/bin and ~/.cargo/bin)" >&2
    exit 1
fi

cd "$(dirname "$0")/.."

profile="${SP404_CARGO_PROFILE:-debug}"
case "$profile" in
    release) set -- --release ;;
    debug) set -- ;;
    *) echo "error: SP404_CARGO_PROFILE must be debug or release, not $profile" >&2; exit 1 ;;
esac

exec cargo build -p sp404-ffi --target aarch64-apple-darwin "$@"
