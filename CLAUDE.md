# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust crate for writing Wayland compositors in Slint, in the spirit of Qt Quick Wayland Compositor. A Wayland server (smithay 0.7) lives in the Slint event loop and hands the windows of its clients to the UI as a model, which `.slint` code shows with `ShellSurfaceItem`.

## Commands

```sh
cargo build --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings   # clean
cargo fmt --check                                                      # clean; rustfmt.toml sets 120 columns
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps -p slint-wayland-compositor   # clean; every public item is documented

cargo test --test protocol               # the compositor against a real Wayland client, no screen needed
cargo test --test protocol -- pointer    # only the tests whose name contains "pointer"

cargo run -p minimal -- weston-terminal        # arguments are shell commands started as clients
cargo run --release -p launcher                # lists the Slint demos it finds next to itself and in PATH
cargo run --release -p launcher -- --fill printerdemo   # full screen mode, start a demo right away

# Features of the library and of examples/launcher
cargo build -p launcher --features bind-wayland-display            # needs the system's libwayland-server
cargo build -p launcher --features backend-linuxkms-noseat,renderer-skia
```

`.github/workflows/ci.yml` runs fmt, clippy, the protocol tests and rustdoc on Ubuntu 24.04 with these commands.

### Tests

`tests/protocol` runs a `Compositor` without UI on Slint's testing backend and talks to it with a real Wayland client on another thread (`tests/protocol/client.rs`). It needs no display, GPU or Wayland session, and uses a runtime directory of its own. It covers what `src/state.rs` does: windows appearing and changing, shared memory content, subsurfaces and popups, pointer, keys, activation, sizing, closing, moves and resizes asked for by clients, maximized/fullscreen/minimized, decoration, the cursor, the output scale, clipboard, primary selection and drag and drop, the limits on buffers and on what the UI can ask for.

It has its own runner (`harness = false`) because the testing backend can be set up only once per process and everything has to happen on the thread of the event loop: a new test is a function added to `TESTS` in `tests/protocol/main.rs`. The `Io error` lines it prints are clients noticing that their compositor is gone, not failures.

Not covered, and to be verified by running an example and looking at it: the `.slint` side (`ShellSurfaceItem`, the examples) and buffers on the GPU.

The demos the launcher starts are not part of this repository. `scripts/slint-demos.sh` clones the Slint release the library is built against into `target/slint` (or uses the checkout `SLINT_DIR` names) and builds them into `target/slint-demos/release`; put that directory in `PATH` when starting the launcher. In Slint, `demos/` and `examples/` are workspaces of their own, not members of the root workspace.
`scripts/slint-demos.sh --bevy` adds the three examples that combine Slint and Bevy (`bevy_example`, `bevy_hosts_slint`, `bevy_hosts_slint_gpu`). They draw with wgpu, so they need the compositor to take dmabufs: a real GPU, which means a board, not Xvfb.

```sh
scripts/slint-demos.sh
PATH=$PWD/target/slint-demos/release:$PATH cargo run --release -p launcher
```

### Running an example to verify a change

Where to show the example depends on where the session runs:

- In a worker VM, use the VM's own display and take the screenshot inside the VM. Its GPU is virtual, so this shows how things look, not how real drivers behave.
- On a board or real GPU, capture its output from outside (HDMI capture or camera), as on the board below.
- On the developer's own machine, do not open windows on the desktop. Use a private display and capture that:

  - `Xvfb :97 -screen 0 1280x800x24` with `DISPLAY=:97` and `WAYLAND_DISPLAY` unset. Input can be driven with `xdotool`, the screen recorded with `ffmpeg -f x11grab`. Slint renders through GLX there, so clients fall back to shared memory: this does not exercise the GPU buffer path.
  - Do not start a second compositor (`kwin_wayland --virtual`, a nested Weston) on that machine either. What needs a real GPU, like buffers on the GPU and clients that draw with wgpu, is verified on a board.

`pkill -f <pattern>` in a command line that itself contains the pattern kills the shell running it. Use `pkill -x`.

## Architecture

### How the `.slint` side reaches users

`ui/wayland-compositor.slint` is compiled by `build.rs` as a library (`as_library("wayland_compositor")`, `rust_module("ui")`), and `Cargo.toml` has `links = "wayland_compositor"`. slint-build passes the location of the file on through the metadata of the `links` key, so a crate that depends on this one can `import { … } from "@wayland_compositor"` with a plain `slint_build::compile()` in its `build.rs`. The generated Rust types are included as `slint_wayland_compositor::ui`.

This relies on the `experimental-module-builds` feature of slint-build, in the library and in the build script of whoever imports it. Consequences:

- The library name must be a Rust identifier, hence the underscore in `@wayland_compositor`.
- A component that uses the compositor has to re-export the global (`export { WaylandCompositor }`). `Compositor::bind()` is generic over the component and requires `ui::WaylandCompositor: slint::Global<Component>`, an impl that Slint generates in the user's crate only for exported globals.

### Rust side

`Compositor` is a handle to `Inner`, which owns the smithay `Display` and `State` (`src/state.rs`) behind a `RefCell`.

- **Event loop**: two futures spawned with `slint::spawn_local` wait with `async-io` for the display's poll fd and the listening socket. A 16 ms `slint::Timer` sends frame callbacks. Everything runs on the thread of the Slint event loop.
- **`Inner::with()`** is the only way into the state. It runs a closure, then `State::refresh()`, flushes the clients, releases the borrow and only then delivers events to the UI and requests a redraw. Handlers in `State` never call into the UI; they queue `Event`s and mark toplevels dirty.
- **`Inner::with_deferred()`** is used from the rendering notifier's setup and teardown. Updating models from there makes Slint panic (`RefCell already mutably borrowed` in the winit backend), so the UI is told from a zero-length timer instead.
- **`ToplevelModel`** (`src/lib.rs`) turns events into the model of `WaylandToplevel`. Rows and the nested model of views are updated in place and never replaced, so that the item showing a toplevel survives every update with its state (position, pointer grab, focus).

### Geometry and input

- A toplevel is a list of views (main surface, subsurfaces, popups), bottom most first, positioned relative to the origin of the xdg window geometry. Client side shadows therefore end up outside of the item.
- `ShellSurfaceItem` has one `TouchArea` covering the window geometry plus its popups (`input-*` in `WaylandToplevel`). It forwards positions; hit testing happens in `State::pointer_motion()` with smithay's `Window::surface_under()`.
- Slint key events carry text, not keycodes. `src/keymap.rs` searches the keymap that clients get for a key producing that text; `State::key()` fakes or lifts Shift around the key press when the character needs a different shift level than the user is holding, which happens when the host layout differs from the client keymap.
- Client initiated moves set `moving` on the toplevel; the item then follows the pointer itself (or emits `dragged`) instead of forwarding motion.
- Client initiated resizes set `resizing` and `resize-edges`. The item sends how far the pointer is from where the button went down (`resize-dragged`), `State::resize_drag()` turns that into configures. `resize-edges` outlives `resizing` until the client has drawn in the last size, because the item moves itself by what the size changes when the top or left edge is dragged.
- `maximized`, `fullscreen` and `minimized` say what the client or the UI want; placing the toplevel and answering with `resize()` is up to the UI (see `Frame` in `examples/minimal`). A request of a client is not sent back to it right away: it goes out with the size the UI answers with, or with the next frame if there is no answer. Clients do not restore their size on their own when they are told a size of zero after being maximized, so the UI has to remember it.
- Globals of `.slint` cannot have their callbacks handled by `.slint` code that imports them. Whatever the UI has to react to is therefore data in the model or a property of the global, not a callback.
- The cursor clients ask for is in `WaylandCompositor.cursor-*`: a CSS name that `ShellSurfaceItem` maps to a `MouseCursor`, or an image the item draws at the pointer.
- `Compositor::bind()` keeps the output's size and scale what they are for the window (checked before every frame and by a timer, as Slint does not tell). Sizes and positions are logical everywhere; clients draw their buffers larger.
- Keys that are down are released when another toplevel is activated and when the item loses the focus (`keys-released`), as Slint delivers the releases elsewhere from then on.
- Slint has no multi touch events: touch arrives as a pointer, there is no `wl_touch`.
- While a button is down, Slint keeps delivering the pointer to the `TouchArea` that got the press, so `ShellSurfaceItem` reports positions outside of its own toplevel. For something a client drags to reach another window, every item tells where it is in the window of the UI (`item-moved`, `Compositor::set_item_position()`), and while a drag lasts `State::pointer_motion()` looks up the toplevel at the pointer itself (`toplevel_at()`). It does not know how the items are stacked: where they overlap it takes the toplevel that was activated last. Outside of a drag the pointer stays with the toplevel it was reported to.

### Client buffers (`src/buffer.rs`, `src/gpu.rs`)

- `wl_shm` buffers are copied into a `slint::Image` on every commit. Buffers beyond `MAX_DIMENSION` or `MAX_SHM_PIXELS` (`src/buffer.rs`) are not shown, and neither is one that would take the copies of all surfaces of all clients beyond `Compositor::set_shm_memory_limit()` (1 GiB by default), so that clients cannot have the compositor copy without limit. A surface whose buffer did not fit is remembered in `State::refused` and looked at again when memory was freed or the limit raised (`shm_freed`, in `refresh()`): a surface keeps its buffer until its next commit. Lowering the limit below what is in use drops the largest copies first. The client is not told; the UI gets `Event::BufferRefused` once per surface and reason, which `bind()` prints. Buffers with an impossible stride never arrive: smithay disconnects the client for them.
- Buffers on the GPU become an `EGLImage`, then a texture in the OpenGL context Slint renders with, handed to Slint as a borrowed texture. Two ways in: linux-dmabuf (Mesa), and with the `bind-wayland-display` feature the driver's own protocol via `EGL_WL_bind_wayland_display` (the only way with Vivante drivers, whose clients crash without it).
- The memory of a buffer on the GPU is the client's; what the importer keeps per buffer is an `EGLImage` and a texture. It takes at most `MAX_GPU_BUFFERS` buffers of `MAX_DIMENSION`, which together stay below `Compositor::set_gpu_memory_limit()` (2 GiB by default, counted as four bytes per pixel); buffers of the driver's own protocol count as well. A dmabuf beyond that fails to be created, which the client is told by the protocol. A texture is dropped when OpenGL reports an error while binding the image to it.
- A commit that waits for its texture is remembered by its surface, not by its buffer: what gets imported is what the surface shows when Slint renders. Commits that were replaced before that are never shown, by design.
- `Compositor::bind()` takes the window's rendering notifier for this. A component cannot use the notifier for something else.
- Textures can only be created while Slint's context is current. A commit with a buffer that has no texture yet is queued in `pending_imports` and imported in `before_rendering()`. A buffer that comes back is the same image to Slint, so `needs_redraw` forces a redraw.
- FemtoVG takes borrowed textures for not premultiplied, while Wayland buffers are: translucent parts of buffers on the GPU come out too dark with it. Slint has no way to say otherwise; Skia and shared memory buffers are fine.
- The importer needs Slint on EGL with OpenGL ES (winit on Wayland, linuxkms; FemtoVG or Skia OpenGL). Otherwise nothing is advertised and clients use shared memory. `Compositor::gpu_buffers_unavailable_reason()` tells which.

## Things that are easy to get wrong

- What comes from the UI or the application is validated where it enters, in `src/lib.rs`: lengths go through `pixels()` (not finite counts as zero, clamped to `MAX_SIZE`), positions that are not finite are dropped. What comes from clients is checked with checked arithmetic.
- `server-decorated` is read from the state the client has acknowledged and committed, so it follows a change of the decoration mode with the commit that answers it. Marking the toplevel dirty when the mode is requested would not change what is read.
- `Compositor::spawn_program()` and `WaylandCompositor.launch-program()` start a program with arguments. `spawn_shell_command()` and `launch-shell-command()` run a shell command line and are named for it: they are for text the application made up itself.

- `.slint` has no hexadecimal number literals, and a component must be declared before the component that uses it.
- The user's global git ignore contains `build`: a directory of that name is silently not tracked.
- `examples/launcher` is derived from `demos/launcher` of Slint and carries its copyright header. The font and the background images in `ui/assets` come from the Slint repository; see the README there. The images are brand material of Slint (CC-BY-ND-4.0): they stay byte for byte what they are in Slint.

## Embedded targets

`cross/cargo.sh` runs cargo for `aarch64-unknown-linux-gnu` in a container (`cross/Dockerfile`: Debian bookworm with the cross toolchain and the arm64 libraries Slint's linuxkms backend links to; needs Docker, the image is built on first use). Results are in `target/cross/aarch64-unknown-linux-gnu/release`.

```sh
cross/cargo.sh build --release -p launcher --features backend-linuxkms-noseat,bind-wayland-display,renderer-skia
scripts/slint-demos.sh --cross                                   # the demos, for the board
scripts/slint-demos.sh --cross --features slint/renderer-skia    # the same with Skia
```

The launcher has been run this way without X11 or Weston on an STM32MP257F-DK (Vivante GPU) and on a Raspberry Pi 5 (V3D, Mesa).

What a board needs at run time (Debian names; Raspberry Pi OS Lite has none of it, and Slint panics in fontique when fontconfig finds no font at all):

```sh
sudo apt-get install --no-install-recommends libegl1 libgles2 libgbm1 libgl1-mesa-dri \
    libinput10 libxkbcommon0 libwayland-client0 libwayland-egl1 libwayland-cursor0 libwayland-server0 \
    libfontconfig1 fonts-dejavu-core
sudo apt-get install --no-install-recommends mesa-vulkan-drivers    # for clients that draw with wgpu, like the Bevy examples
```

### STM32MP257F-DK

- `SLINT_BACKEND=linuxkms-skia-opengl`, and `SLINT_DRM_MODE=<index>` to pick a mode (`list` prints them). Clients are started with `SLINT_BACKEND` set to `SLINT_CLIENT_BACKEND` (default `winit`), because the launcher's backend does not suit them. Demos built with Skia need `SLINT_CLIENT_BACKEND=winit-skia-opengl` there: left to itself, Skia picks wgpu and the client crashes in the driver.
- That GPU is slow at filling the screen. FemtoVG at 1920x1080 gives single digit frame rates; Skia at 1280x720 gives 60 for the launcher alone. With a Slint demo animating as a client in full screen mode, the client runs at 38 (printerdemo) to 46 (energy-monitor) frames per second when it draws with Skia and at 30 with FemtoVG, and the launcher keeps up with it when it redraws every frame as well (measured 2026-11-15). The launcher's background is drawn into a cached layer and clipped to what the windows leave visible for this reason: avoid adding full screen layers or animations.
- Slint demos take several seconds to show their first frame on that board, also under Weston.

### Raspberry Pi 5

Raspberry Pi OS Lite (trixie, glibc 2.41, kernel 6.18, Mesa 26.2). Display and GPU are separate DRM devices (`vc4-drm` and `v3d`); Slint's linuxkms backend finds the display by itself and EGL on its GBM device renders with V3D. `backend-linuxkms-noseat` works as a user in the `video`, `render` and `input` groups, with the kernel console on the screen before and after.

- Clients use linux-dmabuf (seen with `WAYLAND_DEBUG=1`: `zwp_linux_buffer_params_v1.create_immed`, no `wl_shm` buffers); `bind-wayland-display` is not needed.
- The monitor's preferred mode may be 3840x2160 at 30 Hz: pick one with `SLINT_DRM_MODE` (`list` shows them).
- With the launcher redrawing every frame and a Slint demo animating as a client, both run at 60 fps at 1280x720 and 1920x1080 with FemtoVG and with Skia, on either side. At 2560x1440 Skia stays at 60, FemtoVG drops to 48.
- The display has more than twenty planes (the HVS scales and blends), one in use.
- The Bevy examples: `bevy_hosts_slint_gpu` runs (Vulkan through V3DV) and brings the launcher down to about 22 fps at 1920x1080, with or without hardware planes: it is the client's own drawing that uses up the GPU. `bevy_example` of Slint 1.18.1 shows no window and no error. It asks wgpu for the default limits, of which V3DV does not meet `max_inter_stage_shader_variables` (16, V3DV has 15), so creating the device fails; and the winit backend of 1.18.1 keeps that error to itself and goes on without a window. With the limit lowered to 15 in the example it runs. Slint's master reports the error, but its example still asks for the same limits.
- No input device is attached to the board the measurements were made on. A pointer and a keyboard made with uinput (absolute axes for the pointer; relative motion goes through libinput's acceleration) reach the launcher and its clients: clicks start demos and operate them, typed keys end up in a line edit of the widgets gallery.

### reTerminal (Raspberry Pi CM4)

Raspberry Pi OS trixie, V3D 4.2 with Mesa 25.0, a 720x1280 DSI panel with a touch screen (`seeed-tp`). The desktop has been purged from it (`rpd-*`, lightdm, labwc, browsers, with autoremove; network-manager, ssh, Mesa, libinput, xkbcommon, Wayland, fontconfig and ffmpeg marked as manually installed first) and it boots to `multi-user.target`.

- The launcher starts at boot: `examples/launcher/deploy/reterminal/` has the systemd unit (user `pi`, on tty1 instead of its getty, binaries in `/opt/slint-demo`, which the launcher lists) and the udev rule for touches. It is up 18 s after the kernel. `sudo systemctl stop slint-launcher` frees the display for experiments.
- `SLINT_KMS_ROTATION=90` gives the 1280x720 landscape the case is made for. Launcher and a Slint demo run at 60 fps, the client with dmabufs.
- `SLINT_KMS_ROTATION` rotates what is drawn, not where touches are. libinput has to rotate those: `LIBINPUT_CALIBRATION_MATRIX="0 1 0 -1 0 1"` as udev property of the touch screen, for this rotation on this panel.
- What the panel shows is read back from the display controller: `sudo ffmpeg -device /dev/dri/by-path/platform-gpu-card -f kmsgrab -i - -vf hwdownload,format=bgr0 -frames:v 1 shot.png`, in the panel's orientation. Which of `card0` and `card1` is the display changes between boots; the by-path name does not.
- Vulkan of V3D 4.2 leaves the fourth byte of XRGB8888 buffers at zero. Shown as alpha, that made Bevy's window invisible; `GpuImporter` reads alpha as one for formats without alpha.
- Bevy 0.19 draws no meshes on V3D 4.2, here and under labwc alike, with Slint drawing on the GPU or in software, without MSAA and without shadow maps, and without any warning: `bevy_hosts_slint_gpu` shows its background and its text, not the cube. Its OpenGL ES backend is not built into the examples. Not investigated further.
