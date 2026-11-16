//! The Wayland side of the compositor: protocol state and smithay handlers.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use slint::Image;
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::input::{Axis, AxisSource, ButtonState, KeyState};
use smithay::backend::renderer::utils::{on_commit_buffer_handler, CommitCounter, RendererSurfaceStateUserData};
use smithay::backend::renderer::{buffer_type, BufferType};
use smithay::desktop::{
    find_popup_root_surface, PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Window,
    WindowSurfaceType,
};
use smithay::input::keyboard::{FilterResult, Keycode, XkbConfig};
use smithay::input::pointer::{AxisFrame, ButtonEvent, CursorImageStatus, CursorImageSurfaceData, Focus, MotionEvent};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason, ObjectId};
use smithay::reexports::wayland_server::protocol::{
    wl_buffer::WlBuffer, wl_data_source::WlDataSource, wl_seat::WlSeat, wl_surface::WlSurface,
};
use smithay::reexports::wayland_server::{Client, DisplayHandle, Resource};
use smithay::utils::{Logical, Point, Rectangle, Serial, Size, Transform, SERIAL_COUNTER};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    get_parent, is_sync_subsurface, send_surface_state, with_states, with_surface_tree_upward, CompositorClientState,
    CompositorHandler, CompositorState, TraversalAction,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier};
use smithay::wayland::fractional_scale::{with_fractional_scale, FractionalScaleHandler, FractionalScaleManagerState};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::data_device::{
    set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::decoration::{XdgDecorationHandler, XdgDecorationState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::wayland::tablet_manager::TabletSeatHandler;
use smithay::wayland::viewporter::ViewporterState;

use crate::buffer::{image_from_buffer, shm_bytes, DEFAULT_SHM_MEMORY_LIMIT, SHM_FORMATS};
use crate::gpu::GpuImporter;
use crate::keymap::Keymap;
use crate::{BufferRefusal, Cursor, Event, RefusalReason, Toplevel, View};

/// Per client data required by smithay.
#[derive(Default)]
pub struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

struct ToplevelEntry {
    id: u32,
    /// The process on the other end of the connection the toplevel came through.
    pid: u32,
    window: Window,
    /// The UI has been told about this toplevel, which happens once it has content.
    announced: bool,
    /// The client asked for an interactive move that lasts until the button is released.
    moving: bool,
    /// What the client or the UI want the toplevel to be. Giving it the size that goes
    /// with it is up to the UI.
    maximized: bool,
    fullscreen: bool,
    minimized: bool,
    /// The edges (values of `xdg_toplevel.resize_edge`) that follow the pointer, and after
    /// that until the client has caught up with the last size it was asked for.
    resize_edges: u32,
    /// Frames left for the client to catch up with the size the resize ended with.
    resize_settling: Option<(Size<i32, Logical>, u32)>,
}

/// The user is resizing a toplevel at the request of its client.
struct ResizeGrab {
    id: u32,
    edges: u32,
    initial_size: Size<i32, Logical>,
}

const EDGE_TOP: u32 = 1;
const EDGE_BOTTOM: u32 = 2;
const EDGE_LEFT: u32 = 4;
const EDGE_RIGHT: u32 = 8;
/// How many frames a client has to draw in the size a resize ended with.
const RESIZE_SETTLE_FRAMES: u32 = 30;

impl ToplevelEntry {
    fn toplevel(&self) -> &ToplevelSurface {
        self.window.toplevel().expect("only xdg toplevels are tracked")
    }

    fn surface(&self) -> &WlSurface {
        self.toplevel().wl_surface()
    }
}

pub struct State {
    start: Instant,
    display_handle: DisplayHandle,
    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    #[allow(dead_code)] // keeps the global alive
    xdg_decoration_state: XdgDecorationState,
    #[allow(dead_code)] // keeps the global alive
    cursor_shape_state: CursorShapeManagerState,
    #[allow(dead_code)] // keeps the global alive
    fractional_scale_state: FractionalScaleManagerState,
    #[allow(dead_code)] // keeps the global alive
    viewporter_state: ViewporterState,
    primary_selection_state: PrimarySelectionState,
    shm_state: ShmState,
    seat_state: SeatState<State>,
    data_device_state: DataDeviceState,
    popups: PopupManager,
    seat: Seat<State>,
    output: Output,
    output_size: (i32, i32),
    scale: f64,
    /// What clients want the pointer to look like.
    cursor: CursorImageStatus,
    resize: Option<ResizeGrab>,

    dmabuf_state: DmabufState,
    dmabuf_global: Option<DmabufGlobal>,
    /// Present while Slint renders in a way that can show buffers on the GPU.
    importer: Option<GpuImporter>,
    /// Why clients cannot hand over buffers on the GPU.
    pub gpu_unavailable: Option<String>,
    /// The OpenGL context of Slint is current, as it is while Slint renders.
    rendering: bool,
    /// Surfaces showing a buffer on the GPU that has no texture yet.
    pending_imports: Vec<WlSurface>,
    /// Destroyed buffers whose textures are to be deleted.
    dead_buffers: Vec<ObjectId>,
    /// Something changed that Slint does not see as a change of an image.
    pub needs_redraw: bool,

    keymap: Option<Keymap>,
    /// Keys that are down, by the text Slint reported when they went down.
    pressed_keys: HashMap<String, Keycode>,
    /// Where the item of each toplevel is in the window of the UI, if the UI told.
    item_positions: HashMap<u32, (f32, f32)>,
    /// The toplevels in the order they were activated in, the latest last: the best guess
    /// at how they are stacked that there is.
    activation_order: Vec<u32>,
    /// A client drags something around.
    dragging: bool,
    /// The buttons that are down. A set, so that a press or release that is reported twice
    /// or not at all cannot leave a move or resize going forever.
    buttons_down: HashSet<u32>,

    toplevels: Vec<ToplevelEntry>,
    next_id: u32,
    /// Contents of every surface that has a buffer.
    images: HashMap<ObjectId, (CommitCounter, Image)>,
    /// The memory the copy of the shm buffer of each surface takes.
    shm_memory: HashMap<ObjectId, (usize, WlSurface)>,
    /// How much all of them together may take.
    shm_memory_limit: usize,
    /// Surfaces whose buffer is not shown, and why. Those that did not fit into the limit
    /// are tried again when there is room.
    refused: HashMap<ObjectId, (WlSurface, RefusalReason)>,
    /// There is more room for shm buffers than there was.
    shm_freed: bool,
    /// How much memory the buffers on the GPU may take, at four bytes per pixel.
    gpu_memory_limit: usize,
    /// Toplevels the UI has to be updated about.
    dirty: HashSet<u32>,
    /// Events waiting to be delivered to the UI.
    pub events: Vec<Event>,
}

impl State {
    pub fn new(display_handle: DisplayHandle) -> Self {
        let dh = &display_handle;
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(dh, "seat0");
        // Without a usable xkb installation there is still a pointer.
        let keymap = match seat.add_keyboard(XkbConfig::default(), 400, 25) {
            Ok(_) => Keymap::new(),
            Err(_) => None,
        };
        seat.add_pointer();

        let output = Output::new(
            "slint-0".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Slint".into(),
                model: "Window".into(),
            },
        );
        let _global = output.create_global::<State>(dh);

        let mut state = Self {
            start: Instant::now(),
            compositor_state: CompositorState::new::<State>(dh),
            xdg_shell_state: XdgShellState::new::<State>(dh),
            xdg_decoration_state: XdgDecorationState::new::<State>(dh),
            cursor_shape_state: CursorShapeManagerState::new::<State>(dh),
            fractional_scale_state: FractionalScaleManagerState::new::<State>(dh),
            viewporter_state: ViewporterState::new::<State>(dh),
            primary_selection_state: PrimarySelectionState::new::<State>(dh),
            shm_state: ShmState::new::<State>(dh, SHM_FORMATS),
            data_device_state: DataDeviceState::new::<State>(dh),
            seat_state,
            popups: PopupManager::default(),
            seat,
            output,
            output_size: (1920, 1080),
            scale: 1.0,
            cursor: CursorImageStatus::default_named(),
            resize: None,
            dmabuf_state: DmabufState::new(),
            dmabuf_global: None,
            importer: None,
            gpu_unavailable: Some("Slint has not started to render".into()),
            rendering: false,
            pending_imports: Vec::new(),
            dead_buffers: Vec::new(),
            needs_redraw: false,
            keymap,
            pressed_keys: HashMap::new(),
            item_positions: HashMap::new(),
            activation_order: Vec::new(),
            dragging: false,
            buttons_down: HashSet::new(),
            toplevels: Vec::new(),
            next_id: 1,
            images: HashMap::new(),
            shm_memory: HashMap::new(),
            shm_memory_limit: DEFAULT_SHM_MEMORY_LIMIT,
            refused: HashMap::new(),
            shm_freed: false,
            gpu_memory_limit: crate::gpu::DEFAULT_GPU_MEMORY_LIMIT,
            dirty: HashSet::new(),
            events: Vec::new(),
            display_handle,
        };
        state.update_output();
        state
    }

    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn entry(&self, id: u32) -> Option<&ToplevelEntry> {
        self.toplevels.iter().find(|t| t.id == id)
    }

    /// The toplevel a surface belongs to, be it as subsurface or through a popup.
    fn toplevel_of(&self, surface: &WlSurface) -> Option<u32> {
        let mut root = root_surface(surface);
        if let Some(popup) = self.popups.find_popup(&root) {
            root = find_popup_root_surface(&popup).ok()?;
        }
        self.toplevels.iter().find(|t| *t.surface() == root).map(|t| t.id)
    }

    fn mark_all_dirty(&mut self) {
        self.dirty.extend(self.toplevels.iter().map(|t| t.id));
    }

    /// Sizes the output clients see, which they use to size maximized or fullscreen content.
    pub fn set_output_size(&mut self, width: i32, height: i32) {
        // The one place every way to set the size comes through.
        let size = (width.clamp(1, crate::MAX_SIZE), height.clamp(1, crate::MAX_SIZE));
        if size != self.output_size {
            self.output_size = size;
            self.update_output();
        }
    }

    /// Sets how many pixels of a buffer make up a logical pixel, which is what clients
    /// draw for. Sizes and positions stay logical everywhere.
    pub fn set_output_scale(&mut self, scale: f64) {
        let scale = if scale.is_finite() { scale.clamp(0.5, 8.0) } else { 1.0 };
        if scale == self.scale {
            return;
        }
        self.scale = scale;
        self.update_output();
        for surface in self.all_root_surfaces() {
            self.send_scale(&surface);
        }
    }

    fn update_output(&mut self) {
        // The mode is in pixels, the size the UI gives in logical pixels.
        let (width, height) = self.output_size;
        let pixels = |length: i32| ((length as f64 * self.scale).round() as i32).max(1);
        let mode = Mode { size: (pixels(width), pixels(height)).into(), refresh: 60_000 };
        let scale =
            if self.scale.fract() == 0.0 { Scale::Integer(self.scale as i32) } else { Scale::Fractional(self.scale) };
        let previous = self.output.current_mode().filter(|previous| *previous != mode);
        self.output.change_current_state(Some(mode), Some(Transform::Normal), Some(scale), Some((0, 0).into()));
        self.output.set_preferred(mode);
        // There is one mode, the one of the window.
        if let Some(previous) = previous {
            self.output.delete_mode(previous);
        }
    }

    /// The surfaces at the root of everything that is shown: toplevels, popups, the cursor.
    fn all_root_surfaces(&self) -> Vec<WlSurface> {
        let mut surfaces = Vec::new();
        for entry in &self.toplevels {
            surfaces.push(entry.surface().clone());
            surfaces
                .extend(PopupManager::popups_for_surface(entry.surface()).map(|(popup, _)| popup.wl_surface().clone()));
        }
        if let CursorImageStatus::Surface(surface) = &self.cursor {
            surfaces.push(surface.clone());
        }
        surfaces
    }

    /// Tells the surfaces of a tree which scale to draw for, in the ways they understand.
    fn send_scale(&self, surface: &WlSurface) {
        let scale = self.scale;
        with_surface_tree_upward(
            surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, states, _| {
                send_surface_state(surface, states, scale.ceil() as i32, Transform::Normal);
                with_fractional_scale(states, |fractional| fractional.set_preferred_scale(scale));
            },
            |_, _, _| true,
        );
    }

    /// What the pointer is to look like while it is over a toplevel.
    fn describe_cursor(&self) -> Cursor {
        match &self.cursor {
            CursorImageStatus::Hidden => Cursor::Hidden,
            CursorImageStatus::Named(icon) => Cursor::Named(icon.name()),
            CursorImageStatus::Surface(surface) => {
                let Some((_, image)) = self.images.get(&surface.id()) else { return Cursor::Hidden };
                with_states(surface, |states| {
                    let hotspot = states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .map(|data| data.lock().unwrap().hotspot)
                        .unwrap_or_default();
                    let size = states
                        .data_map
                        .get::<RendererSurfaceStateUserData>()
                        .and_then(|data| data.lock().unwrap().view())
                        .map(|view| view.dst)
                        .unwrap_or_default();
                    Cursor::Image {
                        image: image.clone(),
                        hotspot_x: hotspot.x as f32,
                        hotspot_y: hotspot.y as f32,
                        width: size.w as f32,
                        height: size.h as f32,
                    }
                })
            }
        }
    }

    fn is_cursor(&self, surface: &WlSurface) -> bool {
        matches!(&self.cursor, CursorImageStatus::Surface(cursor) if cursor == surface)
    }

    /// Turns everything that changed since the last call into events for the UI.
    pub fn refresh(&mut self) {
        self.popups.cleanup();
        if std::mem::take(&mut self.shm_freed) {
            self.retry_refused();
        }
        let mut dirty: Vec<u32> = self.dirty.drain().collect();
        dirty.sort_unstable();
        for id in dirty {
            let Some(index) = self.toplevels.iter().position(|t| t.id == id) else { continue };
            let mapped = self.images.contains_key(&self.toplevels[index].surface().id());
            if mapped {
                let info = self.describe(&self.toplevels[index]);
                self.toplevels[index].announced = true;
                self.events.push(Event::ToplevelChanged(info));
            } else if std::mem::take(&mut self.toplevels[index].announced) {
                self.events.push(Event::ToplevelRemoved(id));
            }
        }
    }

    fn describe(&self, entry: &ToplevelEntry) -> Toplevel {
        let toplevel = entry.toplevel();
        let surface = toplevel.wl_surface();
        let geometry = entry.window.geometry();

        // Everything is positioned relative to the origin of the window geometry, so that
        // client side shadows end up outside of the item showing the toplevel.
        let mut views = Vec::new();
        self.collect_views(surface, Point::default() - geometry.loc, false, &mut views);
        let mut input = Rectangle::from_size(geometry.size);
        for (popup, location) in PopupManager::popups_for_surface(surface) {
            let mut popup_geometry = popup.geometry();
            if popup_geometry.size.w <= 0 || popup_geometry.size.h <= 0 {
                // A popup without window geometry is what its surfaces cover.
                popup_geometry = smithay::desktop::utils::bbox_from_surface_tree(popup.wl_surface(), (0, 0));
            }
            self.collect_views(popup.wl_surface(), location - popup_geometry.loc, true, &mut views);
            input = input.merge(Rectangle::new(location, popup_geometry.size));
        }

        let (title, app_id) = with_states(surface, |states| {
            let role = states
                .data_map
                .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                .map(|data| data.lock().unwrap());
            match role {
                Some(role) => (role.title.clone().unwrap_or_default(), role.app_id.clone().unwrap_or_default()),
                None => Default::default(),
            }
        });
        let current = toplevel.current_state();

        Toplevel {
            id: entry.id,
            pid: entry.pid,
            title,
            app_id,
            width: geometry.size.w as f32,
            height: geometry.size.h as f32,
            activated: current.states.contains(xdg_toplevel::State::Activated),
            server_decorated: current.decoration_mode == Some(DecorationMode::ServerSide),
            moving: entry.moving,
            maximized: entry.maximized,
            fullscreen: entry.fullscreen,
            minimized: entry.minimized,
            resizing: self.resize.as_ref().is_some_and(|grab| grab.id == entry.id),
            resize_edges: entry.resize_edges,
            input_x: input.loc.x as f32,
            input_y: input.loc.y as f32,
            input_width: input.size.w as f32,
            input_height: input.size.h as f32,
            views,
        }
    }

    /// Appends the surfaces of a surface tree, bottom most first.
    fn collect_views(&self, surface: &WlSurface, origin: Point<i32, Logical>, popup: bool, views: &mut Vec<View>) {
        let view_of = |states: &smithay::wayland::compositor::SurfaceData| {
            states.data_map.get::<RendererSurfaceStateUserData>().and_then(|data| data.lock().unwrap().view())
        };
        with_surface_tree_upward(
            surface,
            origin,
            |_, states, location| match view_of(states) {
                Some(view) => TraversalAction::DoChildren(*location + view.offset),
                None => TraversalAction::SkipChildren,
            },
            |surface, states, location| {
                let (Some(view), Some((_, image))) = (view_of(states), self.images.get(&surface.id())) else {
                    return;
                };
                let location = *location + view.offset;
                views.push(View {
                    x: location.x as f32,
                    y: location.y as f32,
                    width: view.dst.w as f32,
                    height: view.dst.h as f32,
                    image: image.clone(),
                    popup,
                });
            },
            |_, _, _| true,
        );
    }

    /// Sets how much the copies of all shm buffers may take. With less room than is in
    /// use, the largest buffers go first; they come back when there is room again.
    pub fn set_shm_memory_limit(&mut self, bytes: usize) {
        let more = bytes > self.shm_memory_limit;
        self.shm_memory_limit = bytes;
        let mut held: Vec<(usize, WlSurface)> = self.shm_memory.values().cloned().collect();
        held.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
        let mut total: usize = held.iter().map(|(size, _)| *size).sum();
        for (size, surface) in held {
            if total <= bytes {
                break;
            }
            total -= size;
            self.images.remove(&surface.id());
            self.shm_memory.remove(&surface.id());
            self.refuse(&surface, RefusalReason::MemoryLimit);
        }
        self.mark_all_dirty();
        self.shm_freed |= more;
    }

    /// Sets how much the buffers on the GPU may take. Buffers that exist stay.
    pub fn set_gpu_memory_limit(&mut self, bytes: usize) {
        self.gpu_memory_limit = bytes;
        if let Some(importer) = &mut self.importer {
            importer.set_memory_limit(bytes);
        }
    }

    /// Remembers that the buffer of a surface is not shown, and tells the UI once.
    fn refuse(&mut self, surface: &WlSurface, reason: RefusalReason) {
        let known = self.refused.insert(surface.id(), (surface.clone(), reason));
        if known.is_none_or(|(_, before)| before != reason) {
            let toplevel = self.toplevel_of(surface);
            self.events.push(Event::BufferRefused(BufferRefusal { toplevel, reason }));
        }
    }

    /// Looks again at the buffers that did not fit when they were committed. A surface
    /// keeps its buffer until the next commit, so there is something to look at.
    fn retry_refused(&mut self) {
        let waiting: Vec<WlSurface> = self
            .refused
            .values()
            .filter(|(_, reason)| *reason == RefusalReason::MemoryLimit)
            .map(|(surface, _)| surface.clone())
            .collect();
        for surface in waiting {
            if surface.is_alive() {
                self.update_images(&surface);
                self.dirty.extend(self.toplevel_of(&surface));
            }
        }
    }

    /// Converts the buffers that were committed in a surface tree into images.
    fn update_images(&mut self, surface: &WlSurface) {
        // What the traversal finds out about buffers that are not shown.
        let mut refusals: Vec<(WlSurface, Option<RefusalReason>)> = Vec::new();
        let Self {
            images,
            importer,
            rendering,
            pending_imports,
            needs_redraw,
            shm_memory,
            shm_memory_limit,
            shm_freed,
            ..
        } = self;
        with_surface_tree_upward(
            surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, states, _| {
                let Some(data) = states.data_map.get::<RendererSurfaceStateUserData>() else { return };
                let data = data.lock().unwrap();
                let id = surface.id();
                let commit = data.current_commit();
                let Some(buffer) = data.buffer() else {
                    images.remove(&id);
                    *shm_freed |= shm_memory.remove(&id).is_some();
                    refusals.push((surface.clone(), None));
                    return;
                };

                if !matches!(buffer_type(buffer), Some(BufferType::Shm)) {
                    // A buffer on the GPU. Its texture shows what the client draws without
                    // further ado. A buffer that comes back is the same image to Slint
                    // though, so it has to be told.
                    let image = match importer {
                        Some(importer) if *rendering => importer.import(buffer),
                        Some(importer) => {
                            *needs_redraw = true;
                            let image = importer.image(&buffer.id());
                            if image.is_none() {
                                // Textures can only be created while Slint renders. Until
                                // then the surface keeps showing what it showed. It is the
                                // surface that waits, not this buffer: what gets imported
                                // is what the surface shows when Slint renders, and commits
                                // that were replaced before that are never shown.
                                if !pending_imports.contains(surface) {
                                    pending_imports.push(surface.clone());
                                }
                                return;
                            }
                            image
                        }
                        None => None,
                    };
                    *shm_freed |= shm_memory.remove(&id).is_some();
                    refusals.push((surface.clone(), None));
                    match image {
                        Some(image) => images.insert(id, (commit, image)),
                        None => images.remove(&id),
                    };
                } else if !images.get(&id).is_some_and(|(seen, _)| *seen == commit) {
                    // Every surface of every client counts towards what the copies may
                    // take in total. A buffer that does not fit anymore is not shown.
                    let others: usize =
                        shm_memory.iter().filter(|(other, _)| **other != id).map(|(_, (size, _))| *size).sum();
                    let before = shm_memory.get(&id).map_or(0, |(size, _)| *size);
                    let size = shm_bytes(buffer);
                    let fits = size.is_some_and(|size| others.saturating_add(size) <= *shm_memory_limit);
                    match size.filter(|_| fits).and_then(|size| Some((size, image_from_buffer(buffer)?))) {
                        Some((size, image)) => {
                            *shm_freed |= size < before;
                            shm_memory.insert(id.clone(), (size, surface.clone()));
                            images.insert(id, (commit, image));
                            refusals.push((surface.clone(), None));
                        }
                        None => {
                            *shm_freed |= shm_memory.remove(&id).is_some();
                            images.remove(&id);
                            let reason = if fits { RefusalReason::TooLarge } else { RefusalReason::MemoryLimit };
                            refusals.push((surface.clone(), Some(reason)));
                        }
                    }
                }
            },
            |_, _, _| true,
        );
        for (surface, reason) in refusals {
            match reason {
                Some(reason) => self.refuse(&surface, reason),
                None => {
                    self.refused.remove(&surface.id());
                }
            }
        }
    }

    /// Starts to take buffers on the GPU from clients, if the OpenGL context that is current
    /// can show them.
    ///
    /// # Safety
    ///
    /// See [`GpuImporter::new`]. The context has to be current in `before_rendering()`
    /// and `rendering_teardown()` as well.
    pub unsafe fn rendering_setup(&mut self, get_proc_address: &dyn Fn(&std::ffi::CStr) -> *const std::ffi::c_void) {
        self.rendering_teardown();
        let mut importer = match GpuImporter::new(get_proc_address) {
            Ok(importer) => importer,
            Err(reason) => {
                self.gpu_unavailable = Some(reason);
                return;
            }
        };

        let dmabuf = importer.dmabuf_support().and_then(|(device, formats)| {
            let feedback = DmabufFeedbackBuilder::new(device, formats).build().map_err(|e| e.to_string())?;
            let global =
                self.dmabuf_state.create_global_with_default_feedback::<State>(&self.display_handle, &feedback);
            self.dmabuf_global = Some(global);
            Ok(())
        });
        let driver = importer.bind_wayland_display(&self.display_handle);
        self.gpu_unavailable = match (dmabuf, driver) {
            (Err(dmabuf), Err(driver)) => Some(format!("no dmabufs ({dmabuf}), no protocol of the driver ({driver})")),
            _ => None,
        };
        importer.set_memory_limit(self.gpu_memory_limit);
        self.importer = Some(importer);
    }

    /// Does what had to wait for the OpenGL context of Slint to be current.
    pub fn before_rendering(&mut self) {
        let Some(importer) = &mut self.importer else { return };
        for buffer in self.dead_buffers.drain(..) {
            importer.forget(&buffer);
        }
        self.rendering = true;
        for surface in std::mem::take(&mut self.pending_imports) {
            if surface.is_alive() {
                self.update_images(&surface);
                self.dirty.extend(self.toplevel_of(&surface));
                if self.is_cursor(&surface) {
                    self.events.push(Event::CursorChanged(self.describe_cursor()));
                }
            }
        }
        self.rendering = false;
    }

    /// Stops to take buffers on the GPU, because the OpenGL context is about to go away.
    pub fn rendering_teardown(&mut self) {
        if let Some(global) = self.dmabuf_global.take() {
            self.dmabuf_state.destroy_global::<State>(&self.display_handle, global);
        }
        if self.importer.take().is_some() {
            // The textures are gone, and with them what the images refer to.
            self.images.retain(|_, (_, image)| image.to_rgba8_premultiplied().is_some());
            self.mark_all_dirty();
            self.gpu_unavailable = Some("Slint has stopped to render".into());
        }
        self.pending_imports.clear();
        self.dead_buffers.clear();
    }

    /// Tells clients that it is a good time to draw the next frame.
    pub fn send_frames(&mut self) {
        let time = self.start.elapsed();
        for entry in &mut self.toplevels {
            entry.window.send_frame(&self.output, time, Some(Duration::ZERO), |_, _| Some(self.output.clone()));
            // What a client asked for and the UI did not answer with a size of its own.
            if entry.toplevel().is_initial_configure_sent() {
                entry.toplevel().send_pending_configure();
            }
            if let Some((size, frames)) = &mut entry.resize_settling {
                *frames = frames.saturating_sub(1);
                if *frames == 0 || entry.window.geometry().size == *size {
                    entry.resize_settling = None;
                    entry.resize_edges = 0;
                    self.dirty.insert(entry.id);
                }
            }
        }
        if let CursorImageStatus::Surface(surface) = &self.cursor {
            let output = &self.output;
            smithay::desktop::utils::send_frames_surface_tree(surface, output, time, Some(Duration::ZERO), |_, _| {
                Some(output.clone())
            });
        }
    }

    // Requests from the UI

    /// Tells where the item that shows a toplevel is in the window of the UI.
    pub fn set_item_position(&mut self, id: u32, x: f32, y: f32) {
        self.item_positions.insert(id, (x, y));
    }

    /// The toplevel at a position in the window of the UI, and the position relative to
    /// it. Of toplevels that overlap, it is the one that was activated last.
    fn toplevel_at(&self, x: f32, y: f32) -> Option<(u32, f32, f32)> {
        let recency = |id: u32| self.activation_order.iter().rposition(|other| *other == id);
        self.toplevels
            .iter()
            .filter(|entry| entry.announced && !entry.minimized)
            .filter_map(|entry| {
                let (left, top) = *self.item_positions.get(&entry.id)?;
                let size = entry.window.geometry().size;
                let inside = x >= left && y >= top && x < left + size.w as f32 && y < top + size.h as f32;
                inside.then_some((entry.id, x - left, y - top))
            })
            .max_by_key(|(id, ..)| recency(*id))
    }

    pub fn pointer_motion(&mut self, mut id: u32, mut x: f32, mut y: f32) {
        // While a button is down, the UI reports the pointer to the toplevel the button
        // went down in. What is dragged is meant for the toplevel the pointer is over.
        if self.dragging {
            if let Some((left, top)) = self.item_positions.get(&id) {
                match self.toplevel_at(left + x, top + y) {
                    Some(target) => (id, x, y) = target,
                    None => return self.pointer_leave(),
                }
            }
        }
        let Some(entry) = self.entry(id) else { return };
        if entry.moving || self.resize.is_some() {
            return;
        }
        // The UI works relative to the window geometry, smithay relative to the surface.
        let offset = entry.window.geometry().loc.to_f64();
        let location = Point::<f64, Logical>::from((x as f64, y as f64));
        let focus = entry
            .window
            .surface_under(location + offset, WindowSurfaceType::ALL)
            .map(|(surface, origin)| (surface, origin.to_f64() - offset));
        self.move_pointer(focus, location);
    }

    pub fn pointer_leave(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        let location = pointer.current_location();
        self.move_pointer(None, location);
    }

    fn move_pointer(&mut self, focus: Option<(WlSurface, Point<f64, Logical>)>, location: Point<f64, Logical>) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        let event = MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time: self.time() };
        pointer.motion(self, focus, &event);
        pointer.frame(self);
    }

    pub fn pointer_button(&mut self, button: u32, pressed: bool) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        // Clients are only told about changes.
        let changed = if pressed { self.buttons_down.insert(button) } else { self.buttons_down.remove(&button) };
        if !changed {
            return;
        }
        let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
        let event = ButtonEvent { button, state, serial: SERIAL_COUNTER.next_serial(), time: self.time() };
        pointer.button(self, &event);
        pointer.frame(self);

        if self.buttons_down.is_empty() {
            for entry in self.toplevels.iter_mut().filter(|t| t.moving) {
                entry.moving = false;
                self.dirty.insert(entry.id);
            }
            self.end_resize();
        }
    }

    /// Follows the pointer with the edges a client asked to be resized by. The distance
    /// is how far the pointer is from where the button went down.
    pub fn resize_drag(&mut self, id: u32, dx: i32, dy: i32) {
        let Some(grab) = self.resize.as_ref().filter(|grab| grab.id == id) else { return };
        let (edges, initial) = (grab.edges, grab.initial_size);
        let Some(entry) = self.toplevels.iter().find(|t| t.id == id) else { return };

        let mut size = initial;
        if edges & EDGE_LEFT != 0 {
            size.w = size.w.saturating_sub(dx);
        } else if edges & EDGE_RIGHT != 0 {
            size.w = size.w.saturating_add(dx);
        }
        if edges & EDGE_TOP != 0 {
            size.h = size.h.saturating_sub(dy);
        } else if edges & EDGE_BOTTOM != 0 {
            size.h = size.h.saturating_add(dy);
        }
        let (min, max) = with_states(entry.surface(), |states| {
            let mut cached = states.cached_state.get::<SurfaceCachedState>();
            let current = cached.current();
            (current.min_size, current.max_size)
        });
        let limit = |length: i32, min: i32, max: i32| {
            let length = length.clamp(min.max(1), crate::MAX_SIZE);
            if max > 0 {
                length.min(max)
            } else {
                length
            }
        };
        size.w = limit(size.w, min.w, max.w);
        size.h = limit(size.h, min.h, max.h);

        let toplevel = entry.toplevel();
        toplevel.with_pending_state(|state| {
            state.size = Some(size);
            state.states.set(xdg_toplevel::State::Resizing);
        });
        toplevel.send_pending_configure();
    }

    fn end_resize(&mut self) {
        let Some(grab) = self.resize.take() else { return };
        let Some(entry) = self.toplevels.iter_mut().find(|t| t.id == grab.id) else { return };
        let toplevel = entry.toplevel();
        let size = toplevel.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Resizing);
            state.size
        });
        toplevel.send_pending_configure();
        // The item showing the toplevel keeps the opposite edges in place until the
        // client has drawn in the last size.
        match size {
            Some(size) if entry.window.geometry().size != size => {
                entry.resize_settling = Some((size, RESIZE_SETTLE_FRAMES));
            }
            _ => entry.resize_edges = 0,
        }
        self.dirty.insert(grab.id);
    }

    /// Scrolls by a distance in logical pixels, in the direction Slint reports it.
    pub fn pointer_axis(&mut self, delta_x: f32, delta_y: f32) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        // Slint reports 60 pixels per wheel notch and how far the content moves.
        // Wayland wants 15 units per notch and how far the view moves.
        let mut frame = AxisFrame::new(self.time()).source(AxisSource::Wheel);
        for (axis, delta) in [(Axis::Horizontal, delta_x), (Axis::Vertical, delta_y)] {
            if delta != 0.0 {
                frame = frame.value(axis, -delta as f64 / 4.0).v120(axis, (-delta * 2.0) as i32);
            }
        }
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    pub fn key(&mut self, text: &str, pressed: bool) {
        let Some(keymap) = &self.keymap else { return };
        if !pressed {
            let name = text.to_lowercase();
            let keycode = self.pressed_keys.remove(&name).or_else(|| {
                // The text of a release can differ from the press when modifiers changed in
                // between. Guess rather than leave a key down that the client then repeats.
                let resolved = keymap.resolve(text)?.location.keycode;
                let name = self.pressed_keys.iter().find(|(_, k)| **k == resolved).map(|(n, _)| n.clone())?;
                self.pressed_keys.remove(&name)
            });
            if let Some(keycode) = keycode {
                self.send_key(keycode, false);
            }
            return;
        }

        let Some(resolved) = keymap.resolve(text) else { return };
        let keycode = resolved.location.keycode;
        if self.pressed_keys.values().any(|k| *k == keycode) {
            return;
        }

        // The window showing the compositor may use another keyboard layout than the keymap
        // the clients got. Then the character needs a different shift state than the user
        // is holding, which gets faked around the key press.
        let shift_keys: Vec<Keycode> = self.pressed_keys.values().copied().filter(|k| keymap.is_shift(*k)).collect();
        let wants_shift = resolved.location.level == 1;
        let is_letter = text.chars().all(char::is_alphabetic);
        let fake_shift = (resolved.printable && wants_shift && shift_keys.is_empty()).then(|| keymap.shift()).flatten();
        let lift_shift = resolved.printable && !is_letter && resolved.location.level == 0;

        if lift_shift {
            for key in &shift_keys {
                self.send_key(*key, false);
            }
        }
        if let Some(shift) = fake_shift {
            self.send_key(shift, true);
        }
        self.pressed_keys.insert(text.to_lowercase(), keycode);
        self.send_key(keycode, true);
        if let Some(shift) = fake_shift {
            self.send_key(shift, false);
        }
        if lift_shift {
            for key in &shift_keys {
                self.send_key(*key, true);
            }
        }
    }

    /// Releases every key that is down, as the user's keyboard is busy elsewhere from now
    /// on and nobody would tell the client that the keys went up.
    pub fn release_keys(&mut self) {
        let keys: Vec<Keycode> = self.pressed_keys.drain().map(|(_, keycode)| keycode).collect();
        for keycode in keys {
            self.send_key(keycode, false);
        }
    }

    fn send_key(&mut self, keycode: Keycode, pressed: bool) {
        let Some(keyboard) = self.seat.get_keyboard() else { return };
        let state = if pressed { KeyState::Pressed } else { KeyState::Released };
        let (serial, time) = (SERIAL_COUNTER.next_serial(), self.time());
        keyboard.input::<(), _>(self, keycode, state, serial, time, |_, _, _| FilterResult::Forward);
    }

    /// Gives a toplevel the keyboard focus and makes it the one that looks active.
    pub fn activate(&mut self, id: u32) {
        // An id of a toplevel that is gone must not take the focus from the one that has it.
        if self.entry(id).is_none() {
            return;
        }
        self.activation_order.retain(|other| *other != id);
        self.activation_order.push(id);
        let mut focus = None;
        for entry in &mut self.toplevels {
            let active = entry.id == id;
            if active && std::mem::take(&mut entry.minimized) {
                self.dirty.insert(entry.id);
            }
            let changed = entry.toplevel().with_pending_state(|state| {
                if active {
                    state.states.set(xdg_toplevel::State::Activated)
                } else {
                    state.states.unset(xdg_toplevel::State::Activated)
                }
            });
            if changed && entry.toplevel().is_initial_configure_sent() {
                entry.toplevel().send_pending_configure();
            }
            if active {
                focus = Some(entry.surface().clone());
            }
        }
        if let (Some(keyboard), Some(focus)) = (self.seat.get_keyboard(), focus) {
            if !keyboard.is_grabbed() && keyboard.current_focus().as_ref() != Some(&focus) {
                // Keys that are down belong to the window they were pressed in.
                self.release_keys();
                keyboard.set_focus(self, Some(focus), SERIAL_COUNTER.next_serial());
            }
        }
    }

    pub fn close(&mut self, id: u32) {
        if let Some(entry) = self.entry(id) {
            entry.toplevel().send_close();
        }
    }

    /// Asks for a size, or leaves the size to the client if there is none to ask for.
    pub fn resize(&mut self, id: u32, width: i32, height: i32) {
        let Some(entry) = self.entry(id) else { return };
        let toplevel = entry.toplevel();
        let size = (width > 0 && height > 0).then(|| (width, height).into());
        toplevel.with_pending_state(|state| state.size = size);
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    /// Tells the client whether its toplevel is all there is to see, so that it can drop
    /// its decoration. The size to fill is asked for with `resize()`.
    pub fn set_fullscreen(&mut self, id: u32, fullscreen: bool) {
        self.set_toplevel_state(id, xdg_toplevel::State::Fullscreen, fullscreen, true);
    }

    /// The same for a toplevel that fills what is left of the screen, upon which clients
    /// keep their decoration but drop shadows and rounded corners.
    pub fn set_maximized(&mut self, id: u32, maximized: bool) {
        self.set_toplevel_state(id, xdg_toplevel::State::Maximized, maximized, true);
    }

    /// Marks a toplevel as put aside. Clients are not told, it is the UI that hides it.
    pub fn set_minimized(&mut self, id: u32, minimized: bool) {
        let Some(entry) = self.toplevels.iter_mut().find(|t| t.id == id) else { return };
        entry.minimized = minimized;
        self.dirty.insert(id);
        if minimized {
            self.deactivate(id);
        }
    }

    /// Takes the keyboard away from a toplevel that is not to be used for now, and gives
    /// it to the one that was used before.
    fn deactivate(&mut self, id: u32) {
        let Some(entry) = self.entry(id) else { return };
        let toplevel = entry.toplevel();
        if !toplevel.with_pending_state(|state| state.states.unset(xdg_toplevel::State::Activated)) {
            return;
        }
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
        let next = self.toplevels.iter().rev().find(|t| t.id != id && t.announced && !t.minimized).map(|t| t.id);
        match (next, self.seat.get_keyboard()) {
            (Some(next), _) => self.activate(next),
            (None, _) => self.clear_keyboard_focus(),
        }
    }

    /// Nobody gets keys anymore, and none is down.
    fn clear_keyboard_focus(&mut self) {
        self.release_keys();
        if let Some(keyboard) = self.seat.get_keyboard() {
            if !keyboard.is_grabbed() {
                keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
            }
        }
    }

    /// Sets what a toplevel is to be. Without `send` the client learns about it together
    /// with the size the UI answers with, or with the next frame if there is no answer.
    fn set_toplevel_state(&mut self, id: u32, which: xdg_toplevel::State, on: bool, send: bool) {
        let Some(entry) = self.toplevels.iter_mut().find(|t| t.id == id) else { return };
        match which {
            xdg_toplevel::State::Fullscreen => entry.fullscreen = on,
            xdg_toplevel::State::Maximized => entry.maximized = on,
            _ => {}
        }
        let toplevel = entry.toplevel();
        toplevel.with_pending_state(|state| if on { state.states.set(which) } else { state.states.unset(which) });
        if send && toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
        self.dirty.insert(id);
    }

    fn id_of(&self, toplevel: &ToplevelSurface) -> Option<u32> {
        self.toplevels.iter().find(|t| t.toplevel() == toplevel).map(|t| t.id)
    }
}

fn root_surface(surface: &WlSurface) -> WlSurface {
    let mut root = surface.clone();
    while let Some(parent) = get_parent(&root) {
        root = parent;
    }
    root
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().expect("clients are created with ClientState").compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        // The state of a synchronized subsurface is applied when its parent commits.
        if !is_sync_subsurface(surface) {
            self.update_images(surface);
        }
        self.popups.commit(surface);
        self.send_scale(surface);
        if self.is_cursor(surface) {
            self.events.push(Event::CursorChanged(self.describe_cursor()));
        }

        if let Some(entry) = self.toplevels.iter().find(|t| t.surface() == surface) {
            entry.window.on_commit();
            if !entry.toplevel().is_initial_configure_sent() {
                entry.toplevel().send_configure();
            }
        } else if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface) {
            if !popup.is_initial_configure_sent() {
                // Fails only if the popup is gone already.
                let _ = popup.send_configure();
            }
        }

        if let Some(id) = self.toplevel_of(surface) {
            self.dirty.insert(id);
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.images.remove(&surface.id());
        self.shm_freed |= self.shm_memory.remove(&surface.id()).is_some();
        self.refused.remove(&surface.id());
        self.pending_imports.retain(|pending| pending != surface);
        if self.is_cursor(surface) {
            self.cursor = CursorImageStatus::default_named();
            self.events.push(Event::CursorChanged(self.describe_cursor()));
        }
        // The surface cannot be traced back to its toplevel anymore.
        self.mark_all_dirty();
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, buffer: &WlBuffer) {
        if self.importer.as_ref().is_some_and(|importer| importer.knows(&buffer.id())) {
            self.dead_buffers.push(buffer.id());
            self.needs_redraw = true;
        }
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(&mut self, _global: &DmabufGlobal, dmabuf: Dmabuf, notifier: ImportNotifier) {
        let Some(importer) = &mut self.importer else { return notifier.failed() };
        let image = match importer.check_dmabuf(&dmabuf) {
            Ok(image) => image,
            Err(reason) => {
                // The client is told that it cannot have this buffer, the UI why.
                if let Some(reason) = reason {
                    self.events.push(Event::BufferRefused(BufferRefusal { toplevel: None, reason }));
                }
                return notifier.failed();
            }
        };
        match notifier.successful::<State>() {
            Ok(buffer) => importer.register_dmabuf(buffer.id(), image, &dmabuf),
            Err(_) => importer.destroy_image(image),
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        self.output.enter(surface.wl_surface());
        let id = self.next_id;
        self.next_id += 1;
        let pid = self
            .display_handle
            .get_client(surface.wl_surface().id())
            .and_then(|client| client.get_credentials(&self.display_handle))
            .map_or(0, |credentials| credentials.pid.max(0) as u32);
        self.toplevels.push(ToplevelEntry {
            id,
            pid,
            window: Window::new_wayland_window(surface),
            announced: false,
            moving: false,
            maximized: false,
            fullscreen: false,
            minimized: false,
            resize_edges: 0,
            resize_settling: None,
        });
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(index) = self.toplevels.iter().position(|t| *t.toplevel() == surface) else { return };
        let entry = self.toplevels.remove(index);
        self.item_positions.remove(&entry.id);
        self.activation_order.retain(|other| *other != entry.id);
        if self.resize.as_ref().is_some_and(|grab| grab.id == entry.id) {
            self.resize = None;
        }
        if entry.announced {
            self.events.push(Event::ToplevelRemoved(entry.id));
        }
        // The window that was used before becomes the active one again.
        let was_active = entry.toplevel().current_state().states.contains(xdg_toplevel::State::Activated);
        if !was_active {
            return;
        }
        match self.toplevels.iter().rev().find(|t| t.announced && !t.minimized).map(|t| t.id) {
            Some(next) => self.activate(next),
            None => self.clear_keyboard_focus(),
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        surface.with_pending_state(|state| state.geometry = positioner.get_geometry());
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn popup_destroyed(&mut self, _surface: PopupSurface) {
        self.mark_all_dirty();
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let Some(seat) = Seat::<State>::from_resource(&seat) else { return };
        let popup = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&popup) else { return };
        let Ok(mut grab) = self.popups.grab_popup(root, popup, &seat, serial) else { return };

        let previous = grab.previous_serial().unwrap_or(serial);
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed() && !(keyboard.has_grab(serial) || keyboard.has_grab(previous)) {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed() && !(pointer.has_grab(serial) || pointer.has_grab(previous)) {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: WlSeat, _serial: Serial) {
        if self.buttons_down.is_empty() {
            return;
        }
        let Some(entry) = self.toplevels.iter_mut().find(|t| *t.toplevel() == surface) else { return };
        entry.moving = true;
        self.dirty.insert(entry.id);

        // The UI drags the toplevel around from here on. Like in any other compositor,
        // the client does not see the pointer until that is over.
        if let Some(pointer) = self.seat.get_pointer() {
            let (serial, time) = (SERIAL_COUNTER.next_serial(), self.time());
            pointer.unset_grab(self, serial, time);
            let location = pointer.current_location();
            self.move_pointer(None, location);
        }
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: WlSeat,
        _serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let edges = edges as u32;
        if self.buttons_down.is_empty() || edges == 0 || self.resize.is_some() {
            return;
        }
        let Some(entry) = self.toplevels.iter_mut().find(|t| *t.toplevel() == surface) else { return };
        entry.resize_edges = edges;
        entry.resize_settling = None;
        let id = entry.id;
        self.resize = Some(ResizeGrab { id, edges, initial_size: entry.window.geometry().size });
        self.dirty.insert(id);

        // As with a move, the pointer is none of the client's business meanwhile.
        if let Some(pointer) = self.seat.get_pointer() {
            let (serial, time) = (SERIAL_COUNTER.next_serial(), self.time());
            pointer.unset_grab(self, serial, time);
            let location = pointer.current_location();
            self.move_pointer(None, location);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.id_of(&surface) {
            self.set_toplevel_state(id, xdg_toplevel::State::Maximized, true, false);
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.id_of(&surface) {
            self.set_toplevel_state(id, xdg_toplevel::State::Maximized, false, false);
        }
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        if let Some(id) = self.id_of(&surface) {
            self.set_toplevel_state(id, xdg_toplevel::State::Fullscreen, true, false);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.id_of(&surface) {
            self.set_toplevel_state(id, xdg_toplevel::State::Fullscreen, false, false);
        }
    }

    fn minimize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.id_of(&surface) {
            self.set_minimized(id, true);
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        self.dirty.extend(self.toplevel_of(surface.wl_surface()));
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.dirty.extend(self.toplevel_of(surface.wl_surface()));
    }
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // Leave decorating to the UI for every client that is able to go without.
        self.set_decoration_mode(toplevel, DecorationMode::ServerSide);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        self.set_decoration_mode(toplevel, mode);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.set_decoration_mode(toplevel, DecorationMode::ServerSide);
    }
}

impl State {
    fn set_decoration_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        toplevel.with_pending_state(|state| state.decoration_mode = Some(mode));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|surface| self.display_handle.get_client(surface.id()).ok());
        set_data_device_focus(&self.display_handle, seat, client.clone());
        set_primary_focus(&self.display_handle, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor = image;
        if let CursorImageStatus::Surface(surface) = &self.cursor {
            let surface = surface.clone();
            self.send_scale(&surface);
        }
        self.events.push(Event::CursorChanged(self.describe_cursor()));
    }
}

impl TabletSeatHandler for State {}

impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.scale;
        with_states(&surface, |states| {
            with_fractional_scale(states, |fractional| fractional.set_preferred_scale(scale));
        });
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for State {
    fn started(&mut self, _source: Option<WlDataSource>, _icon: Option<WlSurface>, _seat: Seat<Self>) {
        self.dragging = true;
    }

    fn dropped(&mut self, _target: Option<WlSurface>, _validated: bool, _seat: Seat<Self>) {
        self.dragging = false;
    }
}
impl ServerDndGrabHandler for State {}
impl OutputHandler for State {}

smithay::delegate_compositor!(State);
smithay::delegate_shm!(State);
smithay::delegate_dmabuf!(State);
smithay::delegate_xdg_shell!(State);
smithay::delegate_xdg_decoration!(State);
smithay::delegate_seat!(State);
smithay::delegate_data_device!(State);
smithay::delegate_output!(State);
smithay::delegate_primary_selection!(State);
smithay::delegate_cursor_shape!(State);
smithay::delegate_fractional_scale!(State);
smithay::delegate_viewporter!(State);
