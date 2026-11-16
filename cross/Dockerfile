# Builds for 64 bit ARM boards: `cross/cargo.sh build --release -p launcher --features …`
#
# Debian bookworm, because its C library is old enough for the systems boards usually run
# and it has the libraries Slint's linuxkms backend links to.
FROM debian:bookworm

RUN dpkg --add-architecture arm64 && apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl git pkg-config build-essential crossbuild-essential-arm64 \
    libxkbcommon-dev:arm64 libinput-dev:arm64 libudev-dev:arm64 libgbm-dev:arm64 libseat-dev:arm64 \
    libfontconfig-dev:arm64 libfreetype-dev:arm64 libwayland-dev:arm64 \
    libasound2-dev:arm64 \
    && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --target aarch64-unknown-linux-gnu \
    && chmod -R a+rX /opt/rustup /opt/cargo

ENV CARGO_BUILD_TARGET=aarch64-unknown-linux-gnu \
    CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ \
    PKG_CONFIG_ALLOW_CROSS=1 \
    PKG_CONFIG_LIBDIR=/usr/lib/aarch64-linux-gnu/pkgconfig:/usr/share/pkgconfig
