//! A small desktop: every window of a client floats in the Slint window.
//!
//! Shell command lines given as arguments are started as clients, for example
//! `cargo run -p minimal -- weston-terminal`.

slint::include_modules!();

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ui = MainWindow::new()?;
    let compositor = slint_wayland_compositor::Compositor::new()?;
    compositor.bind(&ui);

    println!("Listening on WAYLAND_DISPLAY={}", compositor.socket_name());
    for command in std::env::args().skip(1) {
        // The user's own command line: this is what the shell variant is for.
        compositor.spawn_shell_command(&command)?;
    }

    let status = compositor.clone();
    slint::Timer::single_shot(std::time::Duration::from_secs(1), move || {
        match status.gpu_buffers_unavailable_reason() {
            None => println!("Clients can use GPU buffers"),
            Some(reason) => println!("Clients have to use shared memory: {reason}"),
        }
    });

    ui.run()?;
    Ok(())
}
