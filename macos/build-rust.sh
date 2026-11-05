#!/bin/sh
# Builds the drive's Rust code for the Mac app. Run by Xcode as build phases (see
# project.yml); it can also be run by hand from anywhere.
#
#   build-rust.sh [lib]   the core (crate sp404-ffi) as a static library for the FSKit
#                         extension: target/aarch64-apple-darwin/<profile>/libsp404_ffi.a
#   build-rust.sh cli     the sp404drive command, which the app carries in
#                         Contents/MacOS: target/aarch64-apple-darwin/<profile>/sp404drive
#
#   SP404_CARGO_PROFILE  debug (default) or release
#
# Only arm64 for now.
set -eu

# Xcode runs build phases with a minimal PATH: look where rustup puts cargo.
PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH"
export PATH
if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo not found (looked in /opt/homebrew/opt/rustup/bin and ~/.cargo/bin)" >&2
    exit 1
fi

cd "$(dirname "$0")/.."

what="${1:-lib}"
case "$what" in
    lib) build="-p sp404-ffi" ;;
    cli) build="-p sp404-drive --bin sp404drive" ;;
    *) echo "error: build-rust.sh takes lib or cli, not $what" >&2; exit 1 ;;
esac

profile="${SP404_CARGO_PROFILE:-debug}"
case "$profile" in
    release) set -- --release ;;
    debug) set -- ;;
    *) echo "error: SP404_CARGO_PROFILE must be debug or release, not $profile" >&2; exit 1 ;;
esac

# $build is split into words on purpose.
# shellcheck disable=SC2086
exec cargo build $build --target aarch64-apple-darwin "$@"
