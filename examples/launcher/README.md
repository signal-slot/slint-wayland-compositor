# Launcher

The demo launcher of Slint (`demos/launcher` in the Slint repository), turned into a Wayland
compositor: the demos it starts are Wayland clients whose windows show up inside the launcher.

It lists the Slint demos and examples it finds next to its own binary and in `PATH`. A script
fetches Slint and builds them:

```sh
scripts/slint-demos.sh
PATH=$PWD/target/slint-demos/release:$PATH cargo run --release -p launcher
```

`scripts/slint-demos.sh --bevy` also builds the examples of Slint that combine it with Bevy.
They render with wgpu and need a compositor that takes dmabufs, which the launcher is when
Slint renders with OpenGL ES on EGL (see `Compositor::gpu_buffers_unavailable_reason()`).

Arguments name demos to start right away, `--fill` starts in full screen mode:

```sh
cargo run --release -p launcher -- --fill printerdemo
```

## On a board, without a windowing system

```sh
cross/cargo.sh build --release -p launcher --features backend-linuxkms-noseat,renderer-skia
scripts/slint-demos.sh --cross
```

Copy `target/cross/aarch64-unknown-linux-gnu/release/launcher` and the demos next to it to
the board, and start the launcher there with `SLINT_BACKEND=linuxkms-skia-opengl`. Add the
`bind-wayland-display` feature for GPUs whose driver has its own protocol for buffers
(Vivante).

`deploy/reterminal/` has what makes a reTerminal (Raspberry Pi CM4) boot into the launcher: a
systemd unit that runs it on `tty1` with the panel rotated to landscape, and a udev rule that
rotates the touches to match. The binaries go to `/opt/slint-demo`.

With Slint 1.18, FemtoVG draws conic gradients turned by a quarter, which puts the red sector
of the Speedometer demo away from its needle. Build that demo with Skia
(`--features slint/renderer-skia`) until Slint 1.19, which has the fix.

## Assets

- `src/` and `ui/launcher.slint` are derived from `demos/launcher` of Slint (MIT).
- `ui/assets/bg-frame-*.svg` are brand material of Slint, copied unchanged from
  `tools/viewer/remote/assets` of the Slint repository, where the launcher of Slint takes them
  from as well. They are used here as Slint's branding guidelines (`logo/README.md` there) have
  it for its logos: unmodified, under CC-BY-ND-4.0, © SixtyFPS GmbH. See the `.license` files.
- `ui/assets/Inter-VariableFont.ttf` is the Inter font (OFL-1.1, see the `.license` file).
