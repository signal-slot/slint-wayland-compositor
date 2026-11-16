//! A Wayland client for the tests. It lives on a thread of its own, does what the test
//! tells it to and writes down what the compositor tells it.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source,
    wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface,
    wl_surface,
};
use wayland_client::{delegate_noop, event_created_child, Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_manager_v1 as primary_manager, zwp_primary_selection_device_v1 as primary_device,
    zwp_primary_selection_offer_v1 as primary_offer, zwp_primary_selection_source_v1 as primary_source,
};
use wayland_protocols::xdg::decoration::zv1::client::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1};
use wayland_protocols::xdg::shell::client::{xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Clone, Debug, PartialEq)]
pub enum PointerEvent {
    Enter(f64, f64),
    Leave,
    Motion(f64, f64),
    Button(u32, bool),
    /// Axis (0 is vertical) and distance.
    Axis(u32, f64),
}

/// What the compositor told the client.
#[derive(Default, Debug)]
pub struct Log {
    /// Size and states of every configure of the toplevel.
    pub configures: Vec<(i32, i32, Vec<u32>)>,
    pub close_requested: bool,
    pub pointer: Vec<PointerEvent>,
    /// Key code and whether it was pressed.
    pub keys: Vec<(u32, bool)>,
    pub keyboard_focus: bool,
    pub frames: u32,
    /// The scale the output was last said to have.
    pub output_scale: i32,
    /// Every selection another client offered and that was read.
    pub clipboard: Vec<String>,
    /// The same for the primary selection.
    pub primary: Vec<String>,
    /// What happened to this client as the target of a drag: "enter x y", "motion x y",
    /// "leave" and "drop".
    pub drag: Vec<String>,
    /// What was dropped on this client.
    pub dropped: Vec<String>,
}

pub struct State {
    log: Arc<Mutex<Log>>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    /// Size and color to draw when the compositor has configured the toplevel.
    content: Option<(i32, i32, u32)>,
    /// Pixels of the buffer per logical pixel.
    pub buffer_scale: i32,
    pointer: Option<wl_pointer::WlPointer>,
    /// Serial of the last time the pointer entered.
    pointer_serial: u32,
    cursor: Option<wl_surface::WlSurface>,
    data_device_manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    data_device: Option<wl_data_device::WlDataDevice>,
    /// What this client offers as selection.
    selection: Option<String>,
    /// File descriptors that have to live until the compositor got them.
    in_flight: Vec<OwnedFd>,
    /// Serial of the last button press, which is what a drag has to refer to.
    button_serial: u32,
    /// What is being dragged over this client.
    drag_offer: Option<wl_data_offer::WlDataOffer>,
    primary_manager: Option<primary_manager::ZwpPrimarySelectionDeviceManagerV1>,
    primary_device: Option<primary_device::ZwpPrimarySelectionDeviceV1>,
    subcompositor: Option<wl_subcompositor::WlSubcompositor>,
    decoration_manager: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    /// Draw in the size the compositor asks for, like a real client. Tests that ask for
    /// sizes nobody could draw turn this off.
    pub follow_size: bool,
    /// A popup and the size and color it draws once it is configured.
    popup: Option<(wl_surface::WlSurface, i32, i32, u32)>,
}

/// Marks the xdg_surface of a popup.
pub struct Popup;

const TEXT: &str = "text/plain;charset=utf-8";

impl State {
    /// Opens a window that shows one color (0xAARRGGBB).
    pub fn show(&mut self, qh: &QueueHandle<Self>, title: &str, width: i32, height: i32, color: u32) {
        let surface = self.compositor.as_ref().expect("wl_compositor").create_surface(qh, ());
        let xdg_surface = self.wm_base.as_ref().expect("xdg_wm_base").get_xdg_surface(&surface, qh, ());
        let toplevel = xdg_surface.get_toplevel(qh, ());
        toplevel.set_title(title.into());
        toplevel.set_app_id("test".into());
        surface.commit();
        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
        self.content = Some((width, height, color));
    }

    /// Draws the window again, in another size or color.
    pub fn paint(&mut self, qh: &QueueHandle<Self>, width: i32, height: i32, color: u32) {
        self.content = Some((width, height, color));
        self.draw(qh);
    }

    pub fn set_title(&mut self, title: &str) {
        self.toplevel.as_ref().expect("a window").set_title(title.into());
        self.surface.as_ref().expect("a window").commit();
    }

    /// Asks the compositor to move the window with the pointer.
    pub fn start_move(&mut self, serial: u32) {
        self.toplevel.as_ref().expect("a window")._move(self.seat.as_ref().expect("wl_seat"), serial);
    }

    /// Asks the compositor to resize the window with the pointer.
    pub fn start_resize(&mut self, edges: xdg_toplevel::ResizeEdge) {
        self.toplevel.as_ref().expect("a window").resize(self.seat.as_ref().expect("wl_seat"), 0, edges);
    }

    pub fn toplevel(&self) -> &xdg_toplevel::XdgToplevel {
        self.toplevel.as_ref().expect("a window")
    }

    /// Makes the pointer a square of one color, or hides it.
    pub fn set_cursor(&mut self, qh: &QueueHandle<Self>, cursor: Option<(i32, u32, i32)>) {
        let pointer = self.pointer.as_ref().expect("wl_pointer");
        let Some((size, color, hotspot)) = cursor else {
            return pointer.set_cursor(self.pointer_serial, None, 0, 0);
        };
        let surface = self.compositor.as_ref().expect("wl_compositor").create_surface(qh, ());
        pointer.set_cursor(self.pointer_serial, Some(&surface), hotspot, hotspot);
        let buffer = self.buffer(qh, size, size, color);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        self.cursor = Some(surface);
    }

    /// Offers text as the selection, like copying it does.
    pub fn copy(&mut self, qh: &QueueHandle<Self>, text: &str) {
        let source = self.data_device_manager.as_ref().expect("wl_data_device_manager").create_data_source(qh, ());
        source.offer(TEXT.into());
        self.data_device.as_ref().expect("wl_data_device").set_selection(Some(&source), 0);
        self.selection = Some(text.into());
    }

    /// Offers text as the primary selection, like selecting it does.
    pub fn select(&mut self, qh: &QueueHandle<Self>, text: &str) {
        let source = self.primary_manager.as_ref().expect("primary selection").create_source(qh, ());
        source.offer(TEXT.into());
        self.primary_device.as_ref().expect("primary selection device").set_selection(Some(&source), 0);
        self.selection = Some(text.into());
    }

    /// Starts to drag text from the window, with the button that is down.
    pub fn start_drag(&mut self, qh: &QueueHandle<Self>, text: &str) {
        let source = self.data_device_manager.as_ref().expect("wl_data_device_manager").create_data_source(qh, ());
        source.offer(TEXT.into());
        source.set_actions(wl_data_device_manager::DndAction::Copy);
        let surface = self.surface.as_ref().expect("a window");
        self.data_device.as_ref().expect("wl_data_device").start_drag(Some(&source), surface, None, self.button_serial);
        self.selection = Some(text.into());
    }

    /// Says who is to decorate the window: the client, the compositor, or whoever the
    /// compositor prefers.
    pub fn set_decoration(&mut self, qh: &QueueHandle<Self>, client_side: Option<bool>) {
        use zxdg_toplevel_decoration_v1::Mode;
        if self.decoration.is_none() {
            let manager = self.decoration_manager.as_ref().expect("zxdg_decoration_manager_v1");
            self.decoration = Some(manager.get_toplevel_decoration(self.toplevel.as_ref().expect("a window"), qh, ()));
        }
        let decoration = self.decoration.as_ref().unwrap();
        match client_side {
            Some(true) => decoration.set_mode(Mode::ClientSide),
            Some(false) => decoration.set_mode(Mode::ServerSide),
            None => decoration.unset_mode(),
        }
    }

    /// Asks for a buffer whose rows are shorter than its width needs, which is a protocol
    /// error.
    pub fn paint_with_short_rows(&mut self, qh: &QueueHandle<Self>, width: i32, height: i32) {
        let size = (width * height * 4) as usize;
        // SAFETY: plain libc calls on a file descriptor that is owned right away.
        let fd = unsafe {
            let fd = libc::memfd_create(c"test-buffer".as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0, "memfd_create failed");
            assert_eq!(libc::ftruncate(fd, size as libc::off_t), 0, "ftruncate failed");
            OwnedFd::from_raw_fd(fd)
        };
        let pool = self.shm.as_ref().expect("wl_shm").create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(0, width, height, width * 2, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();
        let surface = self.surface.as_ref().expect("a window");
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
    }

    /// Puts a surface of one color on top of the window, at a position relative to it.
    pub fn add_subsurface(&mut self, qh: &QueueHandle<Self>, x: i32, y: i32, width: i32, height: i32, color: u32) {
        let parent = self.surface.as_ref().expect("a window");
        let surface = self.compositor.as_ref().expect("wl_compositor").create_surface(qh, ());
        let subsurface =
            self.subcompositor.as_ref().expect("wl_subcompositor").get_subsurface(&surface, parent, qh, ());
        subsurface.set_position(x, y);
        let buffer = self.buffer(qh, width, height, color);
        surface.attach(Some(&buffer), 0, 0);
        surface.commit();
        // Position and stacking of a subsurface are state of its parent.
        parent.commit();
    }

    /// Opens a popup of one color at a position relative to the window.
    pub fn open_popup(&mut self, qh: &QueueHandle<Self>, x: i32, y: i32, width: i32, height: i32, color: u32) {
        let wm_base = self.wm_base.as_ref().expect("xdg_wm_base");
        let positioner = wm_base.create_positioner(qh, ());
        positioner.set_size(width, height);
        positioner.set_anchor_rect(x, y, 1, 1);
        positioner.set_anchor(xdg_positioner::Anchor::TopLeft);
        positioner.set_gravity(xdg_positioner::Gravity::BottomRight);
        let surface = self.compositor.as_ref().expect("wl_compositor").create_surface(qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, Popup);
        xdg_surface.get_popup(self.xdg_surface.as_ref(), &positioner, qh, ());
        positioner.destroy();
        surface.commit();
        self.popup = Some((surface, width, height, color));
    }

    pub fn close(&mut self) {
        if let Some(toplevel) = self.toplevel.take() {
            toplevel.destroy();
        }
        if let Some(xdg_surface) = self.xdg_surface.take() {
            xdg_surface.destroy();
        }
        if let Some(surface) = self.surface.take() {
            surface.destroy();
        }
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let (Some(surface), Some((width, height, color))) = (&self.surface, self.content) else { return };
        let scale = self.buffer_scale;
        let buffer = self.buffer(qh, width * scale, height * scale, color);
        surface.set_buffer_scale(scale);
        surface.frame(qh, ());
        surface.attach(Some(&buffer), 0, 0);
        surface.damage_buffer(0, 0, width * scale, height * scale);
        surface.commit();
    }

    fn buffer(&self, qh: &QueueHandle<Self>, width: i32, height: i32, color: u32) -> wl_buffer::WlBuffer {
        let size = (width * height * 4) as usize;
        // SAFETY: plain libc calls on a file descriptor that is owned right away.
        let fd = unsafe {
            let fd = libc::memfd_create(c"test-buffer".as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0, "memfd_create failed");
            OwnedFd::from_raw_fd(fd)
        };
        let pixels: Vec<u8> = color.to_le_bytes().repeat((width * height) as usize);
        let written = unsafe { libc::write(std::os::fd::AsRawFd::as_raw_fd(&fd), pixels.as_ptr().cast(), size) };
        assert_eq!(written, size as isize, "writing the buffer failed");

        let pool = self.shm.as_ref().expect("wl_shm").create_pool(fd.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(0, width, height, width * 4, wl_shm::Format::Argb8888, qh, ());
        pool.destroy();
        buffer
    }
}

type Command = Box<dyn FnOnce(&mut State, &QueueHandle<State>) + Send>;

/// The handle a test has on a client.
pub struct Client {
    commands: Sender<Command>,
    pub log: Arc<Mutex<Log>>,
}

impl Client {
    pub fn connect(socket_name: &str) -> Self {
        let path = std::path::Path::new(&std::env::var("XDG_RUNTIME_DIR").unwrap()).join(socket_name);
        let stream = UnixStream::connect(path).expect("connecting to the compositor");
        let connection = Connection::from_socket(stream).expect("a Wayland connection");
        let log = Arc::new(Mutex::new(Log::default()));
        let (commands, receiver) = channel::<Command>();

        let mut state = State {
            log: log.clone(),
            compositor: None,
            shm: None,
            wm_base: None,
            seat: None,
            surface: None,
            xdg_surface: None,
            toplevel: None,
            content: None,
            buffer_scale: 1,
            pointer: None,
            pointer_serial: 0,
            cursor: None,
            data_device_manager: None,
            data_device: None,
            selection: None,
            in_flight: Vec::new(),
            button_serial: 0,
            drag_offer: None,
            primary_manager: None,
            primary_device: None,
            subcompositor: None,
            decoration_manager: None,
            decoration: None,
            follow_size: true,
            popup: None,
        };
        std::thread::spawn(move || {
            let mut queue = connection.new_event_queue::<State>();
            let qh = queue.handle();
            connection.display().get_registry(&qh, ());
            // Ends when the compositor is gone or the test dropped its handle.
            loop {
                // First of all this is what makes the globals known.
                if queue.roundtrip(&mut state).is_err() {
                    return;
                }
                state.in_flight.clear();
                if let (None, Some(manager), Some(seat)) = (&state.data_device, &state.data_device_manager, &state.seat)
                {
                    state.data_device = Some(manager.get_data_device(seat, &qh, ()));
                }
                if let (None, Some(manager), Some(seat)) = (&state.primary_device, &state.primary_manager, &state.seat)
                {
                    state.primary_device = Some(manager.get_device(seat, &qh, ()));
                }
                loop {
                    match receiver.try_recv() {
                        Ok(command) => command(&mut state, &qh),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        Self { commands, log }
    }

    /// Runs something on the thread of the client.
    pub fn run(&self, command: impl FnOnce(&mut State, &QueueHandle<State>) + Send + 'static) {
        self.commands.send(Box::new(command)).expect("the client is running");
    }

    pub fn log<R>(&self, read: impl FnOnce(&Log) -> R) -> R {
        read(&self.log.lock().unwrap())
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        match interface.as_str() {
            "wl_compositor" => state.compositor = Some(registry.bind(name, version.min(4), qh, ())),
            "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
            "xdg_wm_base" => state.wm_base = Some(registry.bind(name, version.min(3), qh, ())),
            "wl_seat" => state.seat = Some(registry.bind(name, version.min(5), qh, ())),
            "zwp_primary_selection_device_manager_v1" => state.primary_manager = Some(registry.bind(name, 1, qh, ())),
            "zxdg_decoration_manager_v1" => state.decoration_manager = Some(registry.bind(name, 1, qh, ())),
            "wl_subcompositor" => state.subcompositor = Some(registry.bind(name, 1, qh, ())),
            "wl_output" => {
                registry.bind::<wl_output::WlOutput, _, _>(name, version.min(2), qh, ());
            }
            "wl_data_device_manager" => state.data_device_manager = Some(registry.bind(name, version.min(3), qh, ())),
            _ => {}
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            // Like a real client, follow the size the compositor asks for.
            let asked = state.log.lock().unwrap().configures.last().map(|(w, h, _)| (*w, *h));
            if let (Some((width, height)), Some(content)) = (asked, &mut state.content) {
                if width > 0 && height > 0 && state.follow_size {
                    (content.0, content.1) = (width, height);
                }
            }
            state.draw(qh);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut log = state.log.lock().unwrap();
        match event {
            xdg_toplevel::Event::Configure { width, height, states } => {
                let states = states.as_chunks::<4>().0.iter().map(|s| u32::from_ne_bytes(*s)).collect();
                log.configures.push((width, height, states));
            }
            xdg_toplevel::Event::Close => log.close_requested = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(capabilities) } = event {
            if capabilities.contains(wl_seat::Capability::Pointer) {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let event = match event {
            wl_pointer::Event::Enter { serial, surface_x, surface_y, .. } => {
                state.pointer_serial = serial;
                PointerEvent::Enter(surface_x, surface_y)
            }
            wl_pointer::Event::Leave { .. } => PointerEvent::Leave,
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => PointerEvent::Motion(surface_x, surface_y),
            wl_pointer::Event::Button { serial, button, state: WEnum::Value(button_state), .. } => {
                state.button_serial = serial;
                PointerEvent::Button(button, button_state == wl_pointer::ButtonState::Pressed)
            }
            wl_pointer::Event::Axis { axis: WEnum::Value(axis), value, .. } => PointerEvent::Axis(axis as u32, value),
            _ => return,
        };
        state.log.lock().unwrap().pointer.push(event);
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let mut log = state.log.lock().unwrap();
        match event {
            wl_keyboard::Event::Enter { .. } => log.keyboard_focus = true,
            wl_keyboard::Event::Leave { .. } => log.keyboard_focus = false,
            wl_keyboard::Event::Key { key, state: WEnum::Value(key_state), .. } => {
                log.keys.push((key, key_state == wl_keyboard::KeyState::Pressed));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.log.lock().unwrap().frames += 1;
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            buffer.destroy();
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_surface::WlSurface);

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = event {
            state.log.lock().unwrap().output_scale = factor;
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use wl_data_device_manager::DndAction;
        match event {
            // Paste right away whatever becomes the selection.
            wl_data_device::Event::Selection { id: Some(offer) } => {
                let write = read_into(state.log.clone(), |log, text| log.clipboard.push(text));
                offer.receive(TEXT.into(), write.as_fd());
                state.in_flight.push(write);
            }
            wl_data_device::Event::Enter { serial, x, y, id: Some(offer), .. } => {
                offer.accept(serial, Some(TEXT.into()));
                offer.set_actions(DndAction::Copy, DndAction::Copy);
                state.drag_offer = Some(offer);
                state.log.lock().unwrap().drag.push(format!("enter {x} {y}"));
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                state.log.lock().unwrap().drag.push(format!("motion {x} {y}"));
            }
            wl_data_device::Event::Leave => {
                state.drag_offer = None;
                state.log.lock().unwrap().drag.push("leave".into());
            }
            wl_data_device::Event::Drop => {
                state.log.lock().unwrap().drag.push("drop".into());
                if let Some(offer) = state.drag_offer.take() {
                    let write = read_into(state.log.clone(), |log, text| log.dropped.push(text));
                    offer.receive(TEXT.into(), write.as_fd());
                    offer.finish();
                    state.in_flight.push(write);
                }
            }
            _ => {}
        }
    }

    event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_data_source::Event::Send { fd, .. } = event {
            let text = state.selection.clone().unwrap_or_default();
            let _ = std::io::Write::write_all(&mut std::fs::File::from(fd), text.as_bytes());
        }
    }
}

delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);
delegate_noop!(State: ignore wl_data_offer::WlDataOffer);

impl Dispatch<xdg_surface::XdgSurface, Popup> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &Popup,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            if let Some((surface, width, height, color)) = &state.popup {
                let buffer = state.buffer(qh, *width, *height, *color);
                surface.attach(Some(&buffer), 0, 0);
                surface.commit();
            }
        }
    }
}

delegate_noop!(State: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(State: ignore wl_subsurface::WlSubsurface);
delegate_noop!(State: ignore xdg_positioner::XdgPositioner);
delegate_noop!(State: ignore xdg_popup::XdgPopup);
delegate_noop!(State: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
delegate_noop!(State: ignore zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1);

/// A pipe whose content ends up in the log once the other side has written it. Returns
/// the end to write to, which has to live until the compositor got it.
fn read_into(log: Arc<Mutex<Log>>, store: impl FnOnce(&mut Log, String) + Send + 'static) -> OwnedFd {
    let mut fds = [0; 2];
    // SAFETY: plain libc call, the file descriptors are owned right away.
    let (read, write) = unsafe {
        assert_eq!(libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC), 0, "pipe2 failed");
        (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1]))
    };
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut std::fs::File::from(read), &mut text);
        store(&mut log.lock().unwrap(), text);
    });
    write
}

impl Dispatch<primary_device::ZwpPrimarySelectionDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &primary_device::ZwpPrimarySelectionDeviceV1,
        event: primary_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let primary_device::Event::Selection { id: Some(offer) } = event {
            let write = read_into(state.log.clone(), |log, text| log.primary.push(text));
            offer.receive(TEXT.into(), write.as_fd());
            state.in_flight.push(write);
        }
    }

    event_created_child!(State, primary_device::ZwpPrimarySelectionDeviceV1, [
        primary_device::EVT_DATA_OFFER_OPCODE => (primary_offer::ZwpPrimarySelectionOfferV1, ()),
    ]);
}

impl Dispatch<primary_source::ZwpPrimarySelectionSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &primary_source::ZwpPrimarySelectionSourceV1,
        event: primary_source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let primary_source::Event::Send { fd, .. } = event {
            let text = state.selection.clone().unwrap_or_default();
            let _ = std::io::Write::write_all(&mut std::fs::File::from(fd), text.as_bytes());
        }
    }
}

delegate_noop!(State: ignore primary_manager::ZwpPrimarySelectionDeviceManagerV1);
delegate_noop!(State: ignore primary_offer::ZwpPrimarySelectionOfferV1);
