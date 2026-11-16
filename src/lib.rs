//! Write Wayland compositors in Slint, in the spirit of Qt Quick Wayland Compositor.
//!
//! [`Compositor`] is a Wayland server that lives in the Slint event loop. The windows of its
//! clients are handed to the UI as a model of toplevels, which `.slint` code shows with
//! `ShellSurfaceItem` wherever and however it likes.
//!
//! ```ignore
//! // ui/main.slint
//! import { ShellSurfaceItem, WaylandCompositor } from "@wayland_compositor";
//! export { WaylandCompositor }
//!
//! export component MainWindow inherits Window {
//!     for toplevel in WaylandCompositor.toplevels: ShellSurfaceItem {
//!         toplevel: toplevel;
//!     }
//! }
//! ```
//!
//! ```ignore
//! // build.rs, with nothing special: depending on this crate makes the import work
//! fn main() {
//!     slint_build::compile("ui/main.slint").unwrap();
//! }
//! ```
//!
//! ```ignore
//! // src/main.rs
//! slint::include_modules!();
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let ui = MainWindow::new()?;
//!     let compositor = slint_wayland_compositor::Compositor::new()?;
//!     compositor.bind(&ui);
//!     compositor.spawn_program("weston-terminal", [""; 0])?;
//!     ui.run()?;
//!     Ok(())
//! }
//! ```

mod buffer;
mod gpu;
mod keymap;
mod state;

use std::cell::RefCell;
use std::fmt;
use std::os::fd::AsFd;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;

use async_io::Async;
use slint::{Model, VecModel};
use smithay::reexports::wayland_server::{Display, ListeningSocket};

use state::{ClientState, State};

/// The Rust side of what `@wayland_compositor` offers to `.slint` code.
pub mod ui {
    include!(concat!(env!("OUT_DIR"), "/wayland-compositor.rs"));
}

/// One surface of a toplevel: its main surface, a subsurface or a popup.
#[derive(Clone, Debug)]
pub struct View {
    /// Horizontal position relative to the top left corner of the toplevel, in logical pixels.
    pub x: f32,
    /// Vertical position relative to the top left corner of the toplevel, in logical pixels.
    pub y: f32,
    /// Width in logical pixels, which the image is shown at.
    pub width: f32,
    /// Height in logical pixels, which the image is shown at.
    pub height: f32,
    /// What the client drew, usually larger than the logical size when the output is scaled.
    pub image: slint::Image,
    /// The surface belongs to a popup like a menu, and not to the window itself.
    pub popup: bool,
}

/// A window of a client.
#[derive(Clone, Debug)]
pub struct Toplevel {
    /// Identifies the toplevel in the methods of [`Compositor`].
    pub id: u32,
    /// The process of the client, which is what [`Compositor::spawn_program`] returned if
    /// the client was started with it. Zero if it is not known.
    pub pid: u32,
    /// The title the client gave the window.
    pub title: String,
    /// What the client says it is, usually the name of its desktop file.
    pub app_id: String,
    /// Width of the window without shadows, in logical pixels.
    pub width: f32,
    /// Height of the window without shadows, in logical pixels.
    pub height: f32,
    /// The toplevel is the one that got activated last.
    pub activated: bool,
    /// The client agreed to not draw a title bar and borders itself.
    pub server_decorated: bool,
    /// The user is dragging the window around at the request of the client.
    pub moving: bool,
    /// The client or the UI want the toplevel to fill what the screen has to offer, which
    /// the UI answers by asking for that size with [`Compositor::resize`].
    pub maximized: bool,
    /// The same for filling the screen without decoration.
    pub fullscreen: bool,
    /// The client or the UI want the toplevel out of sight until it is activated again.
    pub minimized: bool,
    /// The user is resizing the window at the request of the client: pointer positions
    /// are to go to [`Compositor::resize_drag`].
    pub resizing: bool,
    /// The edges that are being dragged, as in `xdg_toplevel.resize_edge` (1 top, 2 bottom,
    /// 4 left, 8 right, and their sums). While top or left are set, the bottom right
    /// corner is what should stay in place when the size changes.
    pub resize_edges: u32,
    /// Left edge of the area in which pointer events are of interest (the window and its
    /// popups), relative to the toplevel.
    pub input_x: f32,
    /// Top edge of that area, relative to the toplevel.
    pub input_y: f32,
    /// Width of that area.
    pub input_width: f32,
    /// Height of that area.
    pub input_height: f32,
    /// What to show, bottom most first.
    pub views: Vec<View>,
}

/// Something the UI has to react to.
#[derive(Clone, Debug)]
pub enum Event {
    /// A toplevel has appeared or changed. It is identified by [`Toplevel::id`].
    ToplevelChanged(Toplevel),
    /// A toplevel with this id is gone.
    ToplevelRemoved(u32),
    /// The pointer is to look different while it is over a toplevel.
    CursorChanged(Cursor),
    /// A buffer of a client is not shown. The client is not told: to it, its commit went
    /// through. This is for the log of whoever wonders where a window went.
    BufferRefused(BufferRefusal),
}

/// A buffer that is not shown, see [`Event::BufferRefused`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufferRefusal {
    /// The toplevel the buffer belongs to, if it belongs to one. Buffers on the GPU are
    /// refused when they are created, which is before they are used for anything.
    pub toplevel: Option<u32>,
    /// Why the buffer is not shown.
    pub reason: RefusalReason,
}

/// Why a buffer is not shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    /// The buffer alone is larger than what is taken.
    TooLarge,
    /// With this buffer, the buffers of all clients would take more than they may, see
    /// [`Compositor::set_shm_memory_limit`] and [`Compositor::set_gpu_memory_limit`]. A
    /// buffer in shared memory is shown as soon as there is room for it.
    MemoryLimit,
}

/// What a client wants the pointer to look like.
#[derive(Clone, Debug)]
pub enum Cursor {
    /// No pointer at all.
    Hidden,
    /// A pointer of the system, by its CSS name like `default` or `text`.
    Named(&'static str),
    /// A picture of the client.
    Image {
        /// The picture.
        image: slint::Image,
        /// Horizontal position of the pixel of the image that is where the pointer is, in
        /// logical pixels.
        hotspot_x: f32,
        /// Vertical position of that pixel, in logical pixels.
        hotspot_y: f32,
        /// Width to show the image in, in logical pixels.
        width: f32,
        /// Height to show the image in, in logical pixels.
        height: f32,
    },
}

/// Reasons for [`Compositor::new`] to fail.
#[derive(Debug)]
pub enum Error {
    /// The Wayland display could not be set up.
    Display(String),
    /// No socket for clients could be created.
    Socket(String),
    /// The Slint event loop is not available.
    EventLoop(slint::EventLoopError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Display(reason) => write!(f, "cannot create the Wayland display: {reason}"),
            Error::Socket(reason) => write!(f, "cannot create the Wayland socket: {reason}"),
            Error::EventLoop(error) => write!(f, "cannot use the Slint event loop: {error}"),
        }
    }
}

impl std::error::Error for Error {}

/// The largest width or height that is asked of a client or taken for a screen, in logical
/// pixels. Sizes beyond it are clamped.
pub const MAX_SIZE: i32 = 65535;

/// A length or distance from the UI as whole pixels. Whatever is not a finite number
/// counts as zero, and nothing gets beyond [`MAX_SIZE`] in either direction.
fn pixels(length: f32) -> i32 {
    if length.is_finite() {
        length.round().clamp(-(MAX_SIZE as f32), MAX_SIZE as f32) as i32
    } else {
        0
    }
}

type EventHandler = Box<dyn FnMut(Event)>;
type CursorSetter = Box<dyn Fn(Cursor)>;

struct Core {
    display: Display<State>,
    state: State,
}

struct Inner {
    core: RefCell<Core>,
    handler: RefCell<Option<EventHandler>>,
    /// Events went to the handler since this was last reset.
    delivered: std::cell::Cell<bool>,
    /// Makes the window the compositor is bound to render again.
    redraw: RefCell<Option<Box<dyn Fn()>>>,
    socket_name: String,
    frame_timer: slint::Timer,
    /// Keeps the output in sync with the window the compositor is bound to.
    window_timer: slint::Timer,
    tasks: RefCell<Vec<slint::JoinHandle<()>>>,
}

impl Inner {
    /// Runs `f` on the compositor state and takes care of everything that results from it.
    fn with<R>(&self, f: impl FnOnce(&mut Display<State>, &mut State) -> R) -> R {
        let (result, redraw) = {
            let mut core = self.core.borrow_mut();
            let Core { display, state } = &mut *core;
            let result = f(display, state);
            state.refresh();
            // Fails for clients that are gone, which the next dispatch cleans up.
            let _ = display.flush_clients();
            (result, std::mem::take(&mut state.needs_redraw))
        };
        self.deliver_events();
        if redraw {
            if let Some(redraw) = &*self.redraw.borrow() {
                redraw();
            }
        }
        result
    }

    /// Like `with()`, for when Slint is busy with its window and must not be told anything:
    /// the UI learns about what results from `f` once Slint is back in its event loop.
    fn with_deferred(self: &Rc<Self>, f: impl FnOnce(&mut State)) {
        f(&mut self.core.borrow_mut().state);
        let weak = Rc::downgrade(self);
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(inner) = weak.upgrade() {
                inner.with(|_, _| {});
            }
        });
    }

    /// Hands pending events to the handler, which is free to call back into the compositor.
    fn deliver_events(&self) {
        let Some(mut handler) = self.handler.take() else { return };
        loop {
            let events = std::mem::take(&mut self.core.borrow_mut().state.events);
            if events.is_empty() {
                break;
            }
            self.delivered.set(true);
            events.into_iter().for_each(&mut handler);
        }
        // Unless the handler replaced itself.
        self.handler.borrow_mut().get_or_insert(handler);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        for task in self.tasks.take() {
            task.abort();
        }
    }
}

/// A Wayland server whose clients are shown by a Slint UI.
///
/// It serves clients from the Slint event loop of the thread it was created on. Cloning gives
/// another handle to the same compositor, which shuts down when the last handle is dropped.
#[derive(Clone)]
pub struct Compositor {
    inner: Rc<Inner>,
}

impl Compositor {
    /// Starts to listen on the first free `wayland-N` socket in `XDG_RUNTIME_DIR`.
    pub fn new() -> Result<Self, Error> {
        let mut display = Display::<State>::new().map_err(|e| Error::Display(e.to_string()))?;
        let socket = ListeningSocket::bind_auto("wayland", 0..33).map_err(|e| Error::Socket(e.to_string()))?;
        let socket_name = socket
            .socket_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| Error::Socket("the socket has no name".into()))?;

        let io_error = |e: std::io::Error| Error::Display(e.to_string());
        let display_ready =
            Async::new(display.backend().poll_fd().try_clone_to_owned().map_err(io_error)?).map_err(io_error)?;
        let socket_ready = Async::new(socket.as_fd().try_clone_to_owned().map_err(io_error)?).map_err(io_error)?;

        let state = State::new(display.handle());
        let inner = Rc::new(Inner {
            core: RefCell::new(Core { display, state }),
            handler: RefCell::new(None),
            delivered: std::cell::Cell::new(false),
            redraw: RefCell::new(None),
            socket_name,
            frame_timer: slint::Timer::default(),
            window_timer: slint::Timer::default(),
            tasks: RefCell::new(Vec::new()),
        });

        let weak = Rc::downgrade(&inner);
        let requests = slint::spawn_local(async move {
            while display_ready.readable().await.is_ok() {
                let Some(inner) = weak.upgrade() else { break };
                inner.with(|display, state| {
                    // Fails for misbehaving clients only, which get disconnected.
                    let _ = display.dispatch_clients(state);
                });
            }
        })
        .map_err(Error::EventLoop)?;

        let weak = Rc::downgrade(&inner);
        let connections = slint::spawn_local(async move {
            while socket_ready.readable().await.is_ok() {
                let Some(inner) = weak.upgrade() else { break };
                inner.with(|display, _| {
                    while let Ok(Some(stream)) = socket.accept() {
                        let _ = display.handle().insert_client(stream, Arc::new(ClientState::default()));
                    }
                });
            }
        })
        .map_err(Error::EventLoop)?;
        inner.tasks.borrow_mut().extend([requests, connections]);

        // Clients draw when they are told to, which is at about the rate a display refreshes.
        let weak = Rc::downgrade(&inner);
        inner.frame_timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
            if let Some(inner) = Weak::upgrade(&weak) {
                inner.with(|_, state| state.send_frames());
            }
        });

        Ok(Self { inner })
    }

    /// The value of `WAYLAND_DISPLAY` with which clients reach this compositor.
    pub fn socket_name(&self) -> &str {
        &self.inner.socket_name
    }

    /// Starts a shell command line as a client of this compositor. Prefer
    /// [`Compositor::spawn_program`], which takes a program and its arguments.
    ///
    /// `command` is a command line for `sh -c`, with everything a shell does to it. It is
    /// meant for what the application itself or its user chose to run: never build it from
    /// text of unknown origin. `WaylandCompositor.launch-shell-command()` in `.slint` code
    /// ends up here.
    pub fn spawn_shell_command(&self, command: &str) -> std::io::Result<u32> {
        self.spawn_program("sh", ["-c", command])
    }

    /// Starts a program as a client of this compositor, without a shell in between: the
    /// arguments reach the program as they are.
    ///
    /// Returns the id of the process, which the toplevels of the client carry as
    /// [`Toplevel::pid`] unless the program hands over to another process.
    pub fn spawn_program<I, S>(&self, program: impl AsRef<std::ffi::OsStr>, arguments: I) -> std::io::Result<u32>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let mut child =
            std::process::Command::new(program).args(arguments).env("WAYLAND_DISPLAY", self.socket_name()).spawn()?;
        let pid = child.id();
        // Nobody is interested in how it ends, but it has to be reaped.
        std::thread::spawn(move || child.wait());
        Ok(pid)
    }

    /// Sets the function that is told about toplevels. See [`Compositor::bind`] for the usual way.
    ///
    /// Events that happened before are not lost, they are delivered right away.
    pub fn set_event_handler(&self, handler: impl FnMut(Event) + 'static) {
        self.inner.handler.replace(Some(Box::new(handler)));
        self.inner.deliver_events();
    }

    /// Why clients cannot hand over buffers on the GPU and have to use shared memory instead.
    ///
    /// `None` means that they can, which requires [`Compositor::bind`] and Slint to render
    /// with OpenGL ES on EGL. Slint starts to render when the window is shown.
    pub fn gpu_buffers_unavailable_reason(&self) -> Option<String> {
        self.inner.core.borrow().state.gpu_unavailable.clone()
    }

    /// Sets the size clients believe the screen to have, in logical pixels.
    /// [`Compositor::bind`] keeps this and the scale what they are for the window.
    pub fn set_output_size(&self, width: u32, height: u32) {
        // Clamped to [`MAX_SIZE`] by the state, like the size `bind()` takes from the window.
        let whole = |length: u32| i32::try_from(length).unwrap_or(i32::MAX);
        self.inner.with(|_, state| state.set_output_size(whole(width), whole(height)));
    }

    /// Sets how much memory the copies of the shared memory buffers of all clients may
    /// take together, in bytes. The default is 1 GiB; buffers on the GPU are not copied
    /// and do not count.
    ///
    /// A buffer that does not fit is not shown, which [`Event::BufferRefused`] tells, and
    /// is shown when there is room for it. With a limit below what is in use, the largest
    /// buffers stop being shown until the rest fits.
    pub fn set_shm_memory_limit(&self, bytes: usize) {
        self.inner.with(|_, state| state.set_shm_memory_limit(bytes));
    }

    /// Sets how much memory the buffers on the GPU of all clients may take together, in
    /// bytes, counted as four bytes per pixel. The default is 2 GiB.
    ///
    /// The memory is the clients' own, the limit keeps them from asking for all of it.
    /// A client is refused the buffer that does not fit when it creates it, and buffers
    /// that exist stay when the limit is lowered.
    pub fn set_gpu_memory_limit(&self, bytes: usize) {
        self.inner.with(|_, state| state.set_gpu_memory_limit(bytes));
    }

    /// Sets how many pixels clients draw for a logical pixel, like the scale factor of a
    /// Slint window. Sizes and positions are logical everywhere in this crate.
    pub fn set_output_scale(&self, scale: f32) {
        self.inner.with(|_, state| state.set_output_scale(scale as f64));
    }

    /// Tells the client that a toplevel is maximized, or that it is not anymore.
    ///
    /// As with [`Compositor::set_fullscreen`], the size goes with [`Compositor::resize`].
    pub fn set_maximized(&self, id: u32, maximized: bool) {
        self.inner.with(|_, state| state.set_maximized(id, maximized));
    }

    /// Sets [`Toplevel::minimized`], which a client asks for as well. Activating the
    /// toplevel clears it.
    pub fn set_minimized(&self, id: u32, minimized: bool) {
        self.inner.with(|_, state| state.set_minimized(id, minimized));
    }

    /// Moves the pointer while [`Toplevel::resizing`]: the size the client is asked for
    /// follows it. The distance is how far the pointer is from where the button went
    /// down, measured in something that does not move with the toplevel.
    pub fn resize_drag(&self, id: u32, delta_x: f32, delta_y: f32) {
        self.inner.with(|_, state| state.resize_drag(id, pixels(delta_x), pixels(delta_y)));
    }

    /// Releases all keys that are down. To be called when the keyboard focus goes
    /// elsewhere in the UI, as key releases do not arrive from then on.
    pub fn release_keys(&self) {
        self.inner.with(|_, state| state.release_keys());
    }

    /// Makes a toplevel the active one, which is where key events go.
    pub fn activate(&self, id: u32) {
        self.inner.with(|_, state| state.activate(id));
    }

    /// Asks the client to close a toplevel.
    pub fn close(&self, id: u32) {
        self.inner.with(|_, state| state.close(id));
    }

    /// Tells the client that a toplevel fills the screen, or that it does not anymore.
    ///
    /// Clients then usually drop their decoration. What to fill is up to the UI, which asks
    /// for the size with [`Compositor::resize`].
    pub fn set_fullscreen(&self, id: u32, fullscreen: bool) {
        self.inner.with(|_, state| state.set_fullscreen(id, fullscreen));
    }

    /// Asks the client to give a toplevel this size. A size of zero leaves it to the client,
    /// as does one that is not a finite number. Sizes are clamped to [`MAX_SIZE`].
    pub fn resize(&self, id: u32, width: f32, height: f32) {
        self.inner.with(|_, state| state.resize(id, pixels(width), pixels(height)));
    }

    /// Tells where the item that shows a toplevel is, relative to the window of the UI.
    ///
    /// While a button is down, a UI keeps reporting the pointer to the toplevel the button
    /// went down in. With the positions of the items, something a client drags reaches the
    /// toplevel the pointer is over. Where toplevels overlap, that is taken to be the one
    /// that was activated last. `ShellSurfaceItem` tells its position by itself.
    pub fn set_item_position(&self, id: u32, x: f32, y: f32) {
        if x.is_finite() && y.is_finite() {
            self.inner.with(|_, state| state.set_item_position(id, x, y));
        }
    }

    /// Moves the pointer to a position relative to the top left corner of a toplevel.
    pub fn pointer_motion(&self, id: u32, x: f32, y: f32) {
        // A position that is not a number is no position.
        if x.is_finite() && y.is_finite() {
            self.inner.with(|_, state| state.pointer_motion(id, x, y));
        }
    }

    /// Tells clients that the pointer is not over any toplevel anymore.
    pub fn pointer_leave(&self) {
        self.inner.with(|_, state| state.pointer_leave());
    }

    /// Presses or releases a button. `button` is a Linux input event code like `BTN_LEFT`.
    pub fn pointer_button(&self, button: u32, pressed: bool) {
        self.inner.with(|_, state| state.pointer_button(button, pressed));
    }

    /// Scrolls by what a Slint `scroll-event` reports.
    pub fn pointer_axis(&self, delta_x: f32, delta_y: f32) {
        if delta_x.is_finite() && delta_y.is_finite() {
            self.inner.with(|_, state| state.pointer_axis(delta_x, delta_y));
        }
    }

    /// Presses or releases the key that produced the `text` of a Slint `KeyEvent`.
    pub fn key(&self, text: &str, pressed: bool) {
        self.inner.with(|_, state| state.key(text, pressed));
    }
}

/// Keeps the model of toplevels a UI shows in sync with a [`Compositor`].
#[derive(Default)]
struct ToplevelModel {
    toplevels: Rc<VecModel<ui::WaylandToplevel>>,
    /// Id and views of the toplevel in each row.
    rows: RefCell<Vec<(u32, Rc<VecModel<ui::WaylandView>>)>>,
    /// Shows what the pointer is to look like.
    cursor: RefCell<Option<CursorSetter>>,
}

impl ToplevelModel {
    fn handle(&self, event: Event) {
        let mut rows = self.rows.borrow_mut();
        match event {
            Event::ToplevelChanged(toplevel) => {
                // Rows and their models stay what they are, so that the items showing them
                // survive every update, with all the state they have.
                let row = rows.iter().position(|(id, _)| *id == toplevel.id);
                let views = match row {
                    Some(row) => rows[row].1.clone(),
                    None => Rc::new(VecModel::default()),
                };
                for (index, view) in toplevel.views.iter().enumerate() {
                    let view = ui::WaylandView {
                        x: view.x,
                        y: view.y,
                        width: view.width,
                        height: view.height,
                        image: view.image.clone(),
                        popup: view.popup,
                    };
                    if index < views.row_count() {
                        views.set_row_data(index, view);
                    } else {
                        views.push(view);
                    }
                }
                while views.row_count() > toplevel.views.len() {
                    views.remove(views.row_count() - 1);
                }

                let data = ui::WaylandToplevel {
                    id: toplevel.id as i32,
                    pid: toplevel.pid as i32,
                    title: toplevel.title.as_str().into(),
                    app_id: toplevel.app_id.as_str().into(),
                    width: toplevel.width,
                    height: toplevel.height,
                    activated: toplevel.activated,
                    server_decorated: toplevel.server_decorated,
                    moving: toplevel.moving,
                    maximized: toplevel.maximized,
                    fullscreen: toplevel.fullscreen,
                    minimized: toplevel.minimized,
                    resizing: toplevel.resizing,
                    resize_edges: toplevel.resize_edges as i32,
                    input_x: toplevel.input_x,
                    input_y: toplevel.input_y,
                    input_width: toplevel.input_width,
                    input_height: toplevel.input_height,
                    views: views.clone().into(),
                };
                match row {
                    Some(row) => self.toplevels.set_row_data(row, data),
                    None => {
                        rows.push((toplevel.id, views));
                        self.toplevels.push(data);
                    }
                }
            }
            Event::ToplevelRemoved(id) => {
                if let Some(row) = rows.iter().position(|(row_id, _)| *row_id == id) {
                    rows.remove(row);
                    self.toplevels.remove(row);
                }
            }
            Event::CursorChanged(cursor) => {
                if let Some(show) = &*self.cursor.borrow() {
                    show(cursor);
                }
            }
            Event::BufferRefused(refusal) => {
                // Nothing a UI could do about it, but whoever looks for a window that
                // does not show up finds this.
                let whose = refusal.toplevel.map_or("a client".to_string(), |id| format!("toplevel {id}"));
                eprintln!("slint-wayland-compositor: a buffer of {whose} is not shown: {:?}", refusal.reason);
            }
        }
    }
}

impl Compositor {
    /// Connects the compositor to the `WaylandCompositor` global of a Slint component.
    ///
    /// The component has to export the global it imports from `@wayland_compositor`:
    ///
    /// ```slint,ignore
    /// import { ShellSurfaceItem, WaylandCompositor } from "@wayland_compositor";
    /// export { WaylandCompositor }
    /// ```
    ///
    /// This takes the rendering notifier of the window of the component, which is how
    /// buffers on the GPU get shown without copying them.
    pub fn bind<C>(&self, component: &C)
    where
        C: slint::ComponentHandle + 'static,
        for<'a> ui::WaylandCompositor<'a>: slint::Global<'a, C>,
    {
        let global = component.global::<ui::WaylandCompositor>();
        global.set_socket_name(self.socket_name().into());

        let weak = component.as_weak();
        self.inner.redraw.replace(Some(Box::new(move || {
            if let Some(component) = weak.upgrade() {
                component.window().request_redraw();
            }
        })));
        // Clients see a screen that is what the window is. There is no way to be told when
        // the window changes, so this is looked after before every frame and now and then.
        let weak = component.as_weak();
        let follow_window: Rc<dyn Fn(&mut State)> = Rc::new(move |state| {
            let Some(component) = weak.upgrade() else { return };
            let window = component.window();
            let (size, scale) = (window.size(), window.scale_factor());
            if size.width > 0 && size.height > 0 && scale > 0.0 {
                let logical = size.to_logical(scale);
                state.set_output_size(logical.width.round() as i32, logical.height.round() as i32);
                state.set_output_scale(scale as f64);
            }
        });
        self.inner.with(|_, state| follow_window(state));
        let weak = Rc::downgrade(&self.inner);
        let follow = follow_window.clone();
        self.inner.window_timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), move || {
            if let Some(inner) = weak.upgrade() {
                inner.with(|_, state| follow(state));
            }
        });

        let weak = Rc::downgrade(&self.inner);
        // Fails with renderers that have no notifier, with which there are no dmabufs then.
        let _ = component.window().set_rendering_notifier(move |rendering, api| {
            let Some(inner) = weak.upgrade() else { return };
            match rendering {
                slint::RenderingState::RenderingSetup => {
                    if let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = api {
                        // SAFETY: Slint invokes the notifier with its context being current.
                        inner.with_deferred(|state| unsafe { state.rendering_setup(get_proc_address) });
                    }
                }
                slint::RenderingState::BeforeRendering => {
                    inner.delivered.set(false);
                    inner.with(|display, state| {
                        follow_window(state);
                        // What clients have drawn since the last frame belongs into this
                        // one, also when the event loop did not get to read it yet.
                        let _ = display.dispatch_clients(state);
                        state.before_rendering();
                        // Slint is about to render anyway.
                        state.needs_redraw = false;
                    });
                    // The frame that is being rendered shows the new images of toplevels
                    // that have items already. A toplevel that appeared only gets its item
                    // when Slint looks at the model again, which takes another frame. A
                    // client that draws once and then waits, like most do, would not show
                    // up until something else made Slint render.
                    if inner.delivered.get() {
                        let weak = Rc::downgrade(&inner);
                        slint::Timer::single_shot(Duration::ZERO, move || {
                            if let Some(inner) = weak.upgrade() {
                                if let Some(redraw) = &*inner.redraw.borrow() {
                                    redraw();
                                }
                            }
                        });
                    }
                }
                slint::RenderingState::AfterRendering => {
                    // What clients draw now can be in the next frame. The timer alone
                    // would make them wait for the event loop to get around to it, which
                    // takes several frames while the UI animates.
                    inner.with(|_, state| state.send_frames());
                }
                slint::RenderingState::RenderingTeardown => {
                    inner.with_deferred(|state| state.rendering_teardown());
                }
                _ => {}
            }
        });

        let model = ToplevelModel::default();
        global.set_toplevels(model.toplevels.clone().into());
        let weak = component.as_weak();
        model.cursor.replace(Some(Box::new(move |cursor| {
            let Some(component) = weak.upgrade() else { return };
            let global = component.global::<ui::WaylandCompositor>();
            match cursor {
                Cursor::Hidden => global.set_cursor_name("none".into()),
                Cursor::Named(name) => global.set_cursor_name(name.into()),
                Cursor::Image { image, hotspot_x, hotspot_y, width, height } => {
                    global.set_cursor_name("".into());
                    global.set_cursor_image(image);
                    global.set_cursor_hotspot_x(hotspot_x);
                    global.set_cursor_hotspot_y(hotspot_y);
                    global.set_cursor_width(width);
                    global.set_cursor_height(height);
                }
            }
        })));

        self.set_event_handler(move |event| model.handle(event));

        let c = self.clone();
        global.on_activate(move |id| c.activate(id as u32));
        let c = self.clone();
        global.on_close(move |id| c.close(id as u32));
        let c = self.clone();
        global.on_set_fullscreen(move |id, fullscreen| c.set_fullscreen(id as u32, fullscreen));
        let c = self.clone();
        global.on_set_maximized(move |id, maximized| c.set_maximized(id as u32, maximized));
        let c = self.clone();
        global.on_set_minimized(move |id, minimized| c.set_minimized(id as u32, minimized));
        let c = self.clone();
        global.on_resize_dragged(move |id, x, y| c.resize_drag(id as u32, x, y));
        let c = self.clone();
        global.on_keys_released(move || c.release_keys());
        let c = self.clone();
        global.on_item_moved(move |id, x, y| c.set_item_position(id as u32, x, y));
        let c = self.clone();
        global.on_resize(move |id, width, height| c.resize(id as u32, width, height));
        let c = self.clone();
        global.on_pointer_moved(move |id, x, y| c.pointer_motion(id as u32, x, y));
        let c = self.clone();
        global.on_pointer_left(move || c.pointer_leave());
        let c = self.clone();
        global.on_pointer_button(move |button, pressed| c.pointer_button(button as u32, pressed));
        let c = self.clone();
        global.on_pointer_scrolled(move |dx, dy| c.pointer_axis(dx, dy));
        let c = self.clone();
        global.on_key(move |text, pressed| c.key(&text, pressed));
        let c = self.clone();
        global.on_launch_shell_command(move |command| {
            if let Err(error) = c.spawn_shell_command(&command) {
                eprintln!("cannot launch {command}: {error}");
            }
        });
        let c = self.clone();
        global.on_launch_program(move |program, arguments| {
            let arguments: Vec<String> = arguments.iter().map(|argument| argument.to_string()).collect();
            if let Err(error) = c.spawn_program(program.as_str(), &arguments) {
                eprintln!("cannot launch {program}: {error}");
            }
        });
    }
}
