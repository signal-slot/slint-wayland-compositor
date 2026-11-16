# slint-wayland-compositor

Write Wayland compositors in [Slint](https://slint.dev), in the spirit of Qt Quick Wayland Compositor.

A Wayland server lives in the Slint event loop and hands the windows of its clients to the user
interface as a model. `.slint` code shows them with `ShellSurfaceItem`, wherever and however it
likes: floating with a title bar, filling the screen, on a tab.

```slint
import { ShellSurfaceItem, WaylandCompositor } from "@wayland_compositor";
export { WaylandCompositor }

export component MainWindow inherits Window {
    for toplevel in WaylandCompositor.toplevels: ShellSurfaceItem {
        toplevel: toplevel;
    }
}
```

```rust
// build.rs: nothing special, depending on the crate makes the import work
fn main() {
    slint_build::compile("ui/main.slint").unwrap();
}
```

```rust
// src/main.rs
slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ui = MainWindow::new()?;
    let compositor = slint_wayland_compositor::Compositor::new()?;
    compositor.bind(&ui);
    compositor.spawn_program("weston-terminal", [""; 0])?;
    ui.run()?;
    Ok(())
}
```

The crate and the build script of its user need the `experimental-module-builds` feature of
`slint-build`, which is what makes `@wayland_compositor` importable. It is experimental in Slint,
and so is this crate.

## Try it

```sh
cargo run -p minimal -- weston-terminal     # a small desktop with floating windows

scripts/slint-demos.sh                      # fetches Slint and builds its demos
PATH=$PWD/target/slint-demos/release:$PATH cargo run --release -p launcher
```

[`examples/launcher`](examples/launcher) is the demo launcher of Slint turned into a compositor:
the demos it starts are Wayland clients that show up inside it. It also runs without a windowing
system on Slint's linuxkms backend, see its README.

## What works

- xdg-shell windows with subsurfaces and popups, server side and client side decoration.
- Pointer and keyboard, the cursor of clients, moving and resizing started by clients.
- Maximized, fullscreen and minimized, as state the user interface reacts to.
- Clipboard, primary selection and drag and drop between clients.
- Client buffers in shared memory, and on the GPU without a copy: linux-dmabuf, and with the
  `bind-wayland-display` feature the protocol of drivers that have their own (Vivante).
- An output that follows the size and scale of the Slint window.

Tested with Slint 1.18 on a desktop, and without a windowing system on:

| Board | GPU | Result |
|---|---|---|
| Raspberry Pi 5 | V3D 7.1, Mesa | 60 fps at 1920x1080 for the launcher and a Slint demo as client, with FemtoVG and with Skia |
| STM32MP257F-DK | Vivante | Skia at 1280x720: 60 fps for the launcher alone, 38 to 46 with a Slint demo animating with Skia, 30 with FemtoVG |
| reTerminal (Raspberry Pi CM4) | V3D 4.2, Mesa | 60 fps at 1280x720 (rotated panel) for the launcher and a Slint demo as client, with touch; boots into the launcher |

## What does not

- Translucent parts of buffers on the GPU come out too dark with the FemtoVG renderer, which takes
  borrowed textures for not premultiplied. Skia is fine.
- Touch arrives as a pointer: Slint has no multi touch events, so there is no `wl_touch`.
- Keys are found by the text Slint reports for them, not by key code, which Slint does not report.
- No icon follows the pointer while something is dragged. Where windows overlap, what is dragged
  goes to the one that was activated last, which is a guess at what is on top.
- No layer-shell, no XWayland, one output.
- Client buffers go through the GPU. Showing them on hardware planes needs support in Slint's
  linuxkms backend that does not exist yet.

## Tests

```sh
cargo test --test protocol
```

runs the compositor against a real Wayland client, without a screen, GPU or Wayland session.

## License

MIT, see [LICENSE](LICENSE). Slint itself has [its own licenses](https://slint.dev/pricing).

`examples/launcher` is derived from `demos/launcher` of Slint (MIT) and contains the Inter font
(OFL-1.1) and two background images that are brand material of Slint (CC-BY-ND-4.0, unmodified);
see its README.
