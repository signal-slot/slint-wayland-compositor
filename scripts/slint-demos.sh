#!/bin/sh
# Builds the demos and examples of Slint that examples/launcher looks for.
#
#   scripts/slint-demos.sh           for this machine, into target/slint-demos/release
#   scripts/slint-demos.sh --cross   for aarch64 boards, into target/cross/aarch64-unknown-linux-gnu/release
#   scripts/slint-demos.sh --bevy    also the examples that combine Slint and Bevy (--cross comes first)
#
# The Bevy examples take ten minutes and more to build, and 150 to 200 MB each unless stripped.
#
# Put that directory in PATH when starting the launcher. Extra arguments go to cargo, for
# example `--features slint/renderer-skia`.
#
# SLINT_DIR names a checkout of Slint to use. Without it, SLINT_REF (default: the release
# the library is built against) is cloned into target/slint.
set -e
repo=$(cd "$(dirname "$0")/.." && pwd)

cross=
if [ "$1" = "--cross" ]; then
    cross=1
    shift
fi
bevy=
if [ "$1" = "--bevy" ]; then
    bevy=1
    shift
fi

slint=${SLINT_DIR:-$repo/target/slint}
if [ -z "$SLINT_DIR" ]; then
    ref=${SLINT_REF:-v1.19.0}
    if [ ! -d "$slint" ]; then
        git clone --depth 1 --branch "$ref" https://github.com/slint-ui/slint "$slint"
    elif [ "$(git -C "$slint" describe --tags --exact-match 2>/dev/null)" != "$ref" ]; then
        # A checkout of another release, from before the library moved on.
        git -C "$slint" fetch --depth 1 origin tag "$ref"
        git -C "$slint" checkout -q "$ref"
    fi
fi

# In Slint, demos/ and examples/ are workspaces of their own.
build() { # <workspace> <packages...>
    workspace=$1
    shift
    packages=$(printf -- '-p %s ' "$@")
    if [ -n "$cross" ]; then
        CROSS_WORKDIR="$slint" "$repo/cross/cargo.sh" build --release \
            --manifest-path "$slint/$workspace/Cargo.toml" $packages $EXTRA
    else
        cargo build --release --manifest-path "$slint/$workspace/Cargo.toml" \
            --target-dir "$repo/target/slint-demos" $packages $EXTRA
    fi
}
EXTRA="$*"
build demos printerdemo energy-monitor home-automation usecases
build examples gallery memory slide_puzzle carousel speedometer
if [ -n "$bevy" ]; then
    # On its own: built together with the others, cargo gives it their Bevy features, among
    # them a second winit event loop, which it panics about at startup.
    build examples bevy-example
    build examples bevy-hosts-slint bevy-hosts-slint-gpu
fi
