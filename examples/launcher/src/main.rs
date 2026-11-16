// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: MIT

//! The Slint demo launcher as a Wayland compositor.
//!
//! Derived from `demos/launcher` of Slint, which lists the Slint demos and examples that are
//! installed and starts them. Here they do not open windows of their own: they are clients
//! of the compositor this launcher is, and show up inside its window.
//!
//! The demos are looked for next to this binary and in `PATH`.
//!
//! Arguments name demos to start right away, by their binary name, which is of use on
//! devices without anything to click with. `--fill` starts in full screen mode.
//!
//! The demos run with `SLINT_BACKEND` set to the value of `SLINT_CLIENT_BACKEND`, or `winit`
//! if that is not set, because what suits the launcher (like `linuxkms`) does not suit them.

slint::include_modules!();

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::path::PathBuf;

struct CatalogEntry {
    binary: &'static str,
    title: &'static str,
    description: &'static str,
    args: &'static [&'static str],
}

/// The demos and examples this launcher knows about, by installed binary name.
/// Only the ones found in PATH (or next to the launcher) are shown.
const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        binary: "printerdemo",
        title: "Printer Demo",
        description: "A fictional user interface for the touch screen of a printer.",
        args: &[],
    },
    CatalogEntry {
        binary: "energy-monitor",
        title: "Energy Monitor",
        description: "A fictional user interface of a device that monitors energy consumption in a building.",
        args: &[],
    },
    CatalogEntry {
        binary: "home-automation",
        title: "Home Automation",
        description: "A fictional user interface of a device that automates the control of a home.",
        args: &[],
    },
    CatalogEntry {
        binary: "usecases",
        title: "Usecases",
        description: "Different example use cases in one app.",
        args: &[],
    },
    CatalogEntry {
        binary: "gallery",
        title: "Widgets Gallery",
        description: "A gallery of the standard widgets that come with Slint.",
        args: &[],
    },
    CatalogEntry { binary: "todo", title: "Todo", description: "A simple todo list application.", args: &[] },
    CatalogEntry {
        binary: "memory",
        title: "Memory Game",
        description: "The memory game from the Slint tutorial.",
        args: &[],
    },
    CatalogEntry {
        binary: "slide_puzzle",
        title: "Slide Puzzle",
        description: "A sliding tile puzzle game.",
        args: &[],
    },
    CatalogEntry {
        binary: "carousel",
        title: "Carousel",
        description: "A custom carousel widget that can be controlled by touch, mouse, and keyboard.",
        args: &[],
    },
    CatalogEntry {
        binary: "speedometer",
        title: "Speedometer",
        description: "A dashboard with an animated speedometer.",
        args: &[],
    },
    // Slint and Bevy together: these draw with wgpu, and a lot more than the others.
    CatalogEntry {
        binary: "bevy_example",
        title: "Slint hosts Bevy",
        description: "A 3D scene rendered by Bevy, shown inside a Slint user interface.",
        args: &[],
    },
    CatalogEntry {
        binary: "bevy_hosts_slint",
        title: "Bevy hosts Slint",
        description: "A Slint user interface on a cube in a Bevy scene, drawn by the software renderer.",
        args: &[],
    },
    CatalogEntry {
        binary: "bevy_hosts_slint_gpu",
        title: "Bevy hosts Slint (GPU)",
        description: "The same with Skia drawing the user interface on the GPU.",
        args: &[],
    },
    // Clients do not have to be written with Slint.
    CatalogEntry {
        binary: "weston-terminal",
        title: "Terminal",
        description: "Not a Slint application: the terminal of Weston.",
        args: &[],
    },
];

/// Search for `binary` in `search_dirs` and return the first match.
fn find_binary(binary: &str, search_dirs: &[PathBuf]) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    search_dirs
        .iter()
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// Quotes a word for the shell.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut search_dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        search_dirs.extend(exe.parent().map(PathBuf::from));
    }
    if let Some(path) = std::env::var_os("PATH") {
        search_dirs.extend(std::env::split_paths(&path));
    }
    let installed: Vec<(&'static CatalogEntry, PathBuf)> =
        CATALOG.iter().filter_map(|entry| find_binary(entry.binary, &search_dirs).map(|path| (entry, path))).collect();

    let ui = LauncherWindow::new()?;
    let compositor = slint_wayland_compositor::Compositor::new()?;
    compositor.bind(&ui);

    let status = compositor.clone();
    slint::Timer::single_shot(std::time::Duration::from_secs(2), move || {
        match status.gpu_buffers_unavailable_reason() {
            None => eprintln!("Clients can use GPU buffers"),
            Some(reason) => eprintln!("Clients have to use shared memory: {reason}"),
        }
    });

    let launcher = ui.global::<Launcher>();
    launcher.set_entries(ModelRc::new(
        installed
            .iter()
            .map(|(entry, _)| LauncherEntry { title: entry.title.into(), description: entry.description.into() })
            .collect::<VecModel<_>>(),
    ));

    let client_backend = std::env::var("SLINT_CLIENT_BACKEND").unwrap_or_else(|_| "winit".into());
    // The process each entry was started as, to switch to it instead of starting another.
    let running = std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::<&str, u32>::new()));
    let launch = {
        let ui = ui.as_weak();
        move |entry: &CatalogEntry, path: &std::path::Path| {
            let ui = ui.unwrap();
            if let Some(pid) = running.borrow().get(entry.binary).copied() {
                let toplevels = ui.global::<WaylandCompositor>().get_toplevels();
                if let Some(toplevel) = toplevels.iter().find(|toplevel| toplevel.pid == pid as i32) {
                    compositor.activate(toplevel.id as u32);
                    return;
                }
                // Started, and still on its way to its first window.
                if std::path::Path::new(&format!("/proc/{pid}")).exists() {
                    return;
                }
            }
            // `env` sets the backend of the client, without a shell in between.
            let mut arguments = vec![std::ffi::OsString::from(format!("SLINT_BACKEND={client_backend}"))];
            arguments.push(path.into());
            arguments.extend(entry.args.iter().map(std::ffi::OsString::from));
            let status = match compositor.spawn_program("env", arguments) {
                Ok(pid) => {
                    running.borrow_mut().insert(entry.binary, pid);
                    String::new()
                }
                Err(error) => format!("Failed to start {}: {error}", entry.binary),
            };
            ui.global::<Launcher>().set_status(status.into());
        }
    };

    for argument in std::env::args().skip(1) {
        if argument == "--fill" {
            ui.set_fill_mode(true);
        } else if let Some((entry, path)) = installed.iter().find(|(entry, _)| entry.binary == argument) {
            launch(entry, path);
        } else {
            eprintln!("{argument} is not a demo that is installed");
        }
    }

    launcher.on_launch(move |index| {
        if let Some((entry, path)) = installed.get(index as usize) {
            launch(entry, path);
        }
    });

    ui.run()?;
    Ok(())
}
