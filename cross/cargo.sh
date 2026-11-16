#!/bin/sh
# Runs cargo for aarch64-unknown-linux-gnu in a container that has the cross toolchain and
# the libraries of the target:
#
#   cross/cargo.sh build --release -p launcher --features backend-linuxkms-noseat,renderer-skia
#
# The result is in target/cross/aarch64-unknown-linux-gnu/. Set CROSS_WORKDIR to build
# another workspace (a checkout of Slint, for its demos) into the same place.
set -e
repo=$(cd "$(dirname "$0")/.." && pwd)
workdir=${CROSS_WORKDIR:-$repo}
image=slint-wayland-compositor-cross

docker image inspect $image >/dev/null 2>&1 || docker build -t $image "$repo/cross"

mounts="-v $repo:$repo"
[ "$workdir" = "$repo" ] || mounts="$mounts -v $workdir:$workdir"

# Downloaded crates are kept between runs, next to the build results.
mkdir -p "$repo/target/cross/cargo-home"
exec docker run --rm --user "$(id -u):$(id -g)" $mounts -w "$workdir" \
    -e CARGO_HOME="$repo/target/cross/cargo-home" -e HOME="$repo/target/cross/cargo-home" \
    -e CARGO_TARGET_DIR="$repo/target/cross" \
    -e PATH=/opt/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    $image cargo "$@"
