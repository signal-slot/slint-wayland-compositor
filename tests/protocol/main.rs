//! Tests of the compositor against a real Wayland client, without any window on a screen.
//!
//! `cargo test --test protocol` runs all of them, `cargo test --test protocol -- <part of a name>`
//! only those whose name contains the argument.

mod client;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use client::{Client, PointerEvent};
use slint_wayland_compositor::{BufferRefusal, Compositor, Cursor, Event, RefusalReason, Toplevel};
use wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;

/// A compositor without UI, and what it would have told one.
struct Fixture {
    compositor: Compositor,
    toplevels: Rc<RefCell<Vec<Toplevel>>>,
    cursor: Rc<RefCell<Option<Cursor>>>,
    refusals: Rc<RefCell<Vec<BufferRefusal>>>,
}

impl Fixture {
    fn new() -> Self {
        let compositor = Compositor::new().expect("a compositor");
        let toplevels = Rc::new(RefCell::new(Vec::<Toplevel>::new()));
        let model = toplevels.clone();
        let cursor = Rc::new(RefCell::new(None));
        let shown = cursor.clone();
        let refusals = Rc::new(RefCell::new(Vec::new()));
        let refused = refusals.clone();
        compositor.set_event_handler(move |event| {
            let mut toplevels = model.borrow_mut();
            match event {
                Event::ToplevelChanged(toplevel) => match toplevels.iter_mut().find(|t| t.id == toplevel.id) {
                    Some(known) => *known = toplevel,
                    None => toplevels.push(toplevel),
                },
                Event::ToplevelRemoved(id) => toplevels.retain(|t| t.id != id),
                Event::CursorChanged(cursor) => *shown.borrow_mut() = Some(cursor),
                Event::BufferRefused(refusal) => refused.borrow_mut().push(refusal),
            }
        });
        Self { compositor, toplevels, cursor, refusals }
    }

    fn client(&self) -> Client {
        Client::connect(self.compositor.socket_name())
    }

    /// A client with a window, and that window as the compositor sees it.
    fn client_with_window(&self, title: &'static str, width: i32, height: i32, color: u32) -> (Client, Toplevel) {
        let client = self.client();
        client.run(move |state, qh| state.show(qh, title, width, height, color));
        let toplevel = self.wait_for_toplevel(title, |t| t.width == width as f32 && !t.views.is_empty());
        (client, toplevel)
    }

    fn toplevel(&self, title: &str) -> Option<Toplevel> {
        self.toplevels.borrow().iter().find(|t| t.title == title).cloned()
    }

    fn wait_for_toplevel(&self, title: &str, condition: impl Fn(&Toplevel) -> bool) -> Toplevel {
        self.wait(&format!("toplevel {title:?}"), || self.toplevel(title).is_some_and(|t| condition(&t)));
        self.toplevel(title).unwrap()
    }

    /// Lets the event loop run until something is the case.
    fn wait(&self, what: &str, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            run_event_loop_for(Duration::from_millis(5));
        }
    }

    /// Lets the event loop run for a while, for things that are expected not to happen.
    fn settle(&self) {
        run_event_loop_for(Duration::from_millis(150));
    }
}

fn run_event_loop_for(duration: Duration) {
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::SingleShot, duration, || slint::quit_event_loop().unwrap());
    slint::run_event_loop_until_quit().unwrap();
}

/// The color of the first pixel of what a toplevel shows, as 0xAARRGGBB.
fn color_of(toplevel: &Toplevel) -> u32 {
    let pixels = toplevel.views[0].image.to_rgba8_premultiplied().expect("an image with pixels");
    let pixel = pixels.as_slice()[0];
    u32::from_be_bytes([pixel.a, pixel.r, pixel.g, pixel.b])
}

const RED: u32 = 0xffff0000;
const BLUE: u32 = 0xff0000ff;
/// Linux input event codes.
const BTN_LEFT: u32 = 0x110;
const KEY_A: u32 = 30;
const KEY_LEFTSHIFT: u32 = 42;
/// Values of `xdg_toplevel.state`.
const STATE_MAXIMIZED: u32 = 1;
const STATE_FULLSCREEN: u32 = 2;
const STATE_RESIZING: u32 = 3;
const STATE_ACTIVATED: u32 = 4;

fn window_appears_with_its_content() {
    let fixture = Fixture::new();
    let (_client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    assert_eq!((toplevel.width, toplevel.height), (200.0, 100.0));
    assert_eq!(toplevel.app_id, "test");
    // The test client is a thread of this process.
    assert_eq!(toplevel.pid, std::process::id());
    assert_eq!(toplevel.views.len(), 1);
    let view = &toplevel.views[0];
    assert_eq!((view.x, view.y, view.width, view.height), (0.0, 0.0, 200.0, 100.0));
    assert!(!view.popup);
    assert_eq!(color_of(&toplevel), RED);
    assert_eq!(
        (toplevel.input_x, toplevel.input_y, toplevel.input_width, toplevel.input_height),
        (0.0, 0.0, 200.0, 100.0)
    );
}

fn window_follows_what_the_client_draws() {
    let fixture = Fixture::new();
    let (client, _) = fixture.client_with_window("one", 200, 100, RED);
    client.run(|state, qh| state.paint(qh, 120, 80, BLUE));
    let toplevel = fixture.wait_for_toplevel("one", |t| t.width == 120.0);
    assert_eq!(toplevel.height, 80.0);
    assert_eq!(color_of(&toplevel), BLUE);

    client.run(|state, _| state.set_title("renamed"));
    fixture.wait_for_toplevel("renamed", |_| true);
    assert!(fixture.toplevel("one").is_none());
}

fn client_gets_frame_callbacks() {
    let fixture = Fixture::new();
    let (client, _) = fixture.client_with_window("one", 50, 50, RED);
    fixture.wait("a frame callback", || client.log(|log| log.frames >= 1));
}

fn window_goes_away() {
    let fixture = Fixture::new();
    let (client, _) = fixture.client_with_window("one", 50, 50, RED);
    client.run(|state, _| state.close());
    fixture.wait("the toplevel to be removed", || fixture.toplevel("one").is_none());

    // A client that disappears takes its windows with it.
    let (client, _) = fixture.client_with_window("two", 50, 50, RED);
    drop(client);
    fixture.wait("the toplevel of the gone client to be removed", || fixture.toplevel("two").is_none());
}

fn pointer_reaches_the_client() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    let compositor = &fixture.compositor;

    compositor.pointer_motion(toplevel.id, 10.0, 20.0);
    compositor.pointer_motion(toplevel.id, 30.0, 40.0);
    compositor.pointer_button(BTN_LEFT, true);
    compositor.pointer_button(BTN_LEFT, false);
    // What Slint reports for one notch of the wheel away from the user.
    compositor.pointer_axis(0.0, 60.0);
    compositor.pointer_leave();

    fixture.wait("the pointer to leave", || client.log(|log| log.pointer.last() == Some(&PointerEvent::Leave)));
    assert_eq!(
        client.log(|log| log.pointer.clone()),
        [
            PointerEvent::Enter(10.0, 20.0),
            PointerEvent::Motion(30.0, 40.0),
            PointerEvent::Button(BTN_LEFT, true),
            PointerEvent::Button(BTN_LEFT, false),
            PointerEvent::Axis(0, -15.0),
            PointerEvent::Leave,
        ]
    );
}

fn pointer_outside_of_the_window_is_not_on_it() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    fixture.compositor.pointer_motion(toplevel.id, 250.0, 20.0);
    fixture.settle();
    assert_eq!(client.log(|log| log.pointer.clone()), []);
}

fn keys_reach_the_active_window() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    let compositor = &fixture.compositor;

    // Nobody listens before a window is active.
    compositor.key("a", true);
    compositor.key("a", false);
    fixture.settle();
    assert_eq!(client.log(|log| log.keys.clone()), []);

    compositor.activate(toplevel.id);
    fixture.wait("the keyboard focus", || client.log(|log| log.keyboard_focus));
    compositor.key("a", true);
    compositor.key("a", false);
    fixture.wait("the key", || client.log(|log| log.keys.len() == 2));
    assert_eq!(client.log(|log| log.keys.clone()), [(KEY_A, true), (KEY_A, false)]);
}

fn shift_is_added_for_text_that_needs_it() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    let compositor = &fixture.compositor;
    compositor.activate(toplevel.id);
    fixture.wait("the keyboard focus", || client.log(|log| log.keyboard_focus));

    // An upper case letter without the user holding Shift: what a caps lock or another
    // keyboard layout of the host leads to.
    compositor.key("A", true);
    compositor.key("A", false);
    fixture.wait("the keys", || client.log(|log| log.keys.len() == 4));
    assert_eq!(
        client.log(|log| log.keys.clone()),
        [(KEY_LEFTSHIFT, true), (KEY_A, true), (KEY_LEFTSHIFT, false), (KEY_A, false)]
    );

    // With Shift held by the user, nothing is added.
    client.log.lock().unwrap().keys.clear();
    compositor.key("\u{10}", true);
    compositor.key("A", true);
    compositor.key("A", false);
    compositor.key("\u{10}", false);
    fixture.wait("the keys", || client.log(|log| log.keys.len() == 4));
    assert_eq!(
        client.log(|log| log.keys.clone()),
        [(KEY_LEFTSHIFT, true), (KEY_A, true), (KEY_A, false), (KEY_LEFTSHIFT, false)]
    );
}

fn only_one_window_is_active() {
    let fixture = Fixture::new();
    let (first, one) = fixture.client_with_window("one", 100, 100, RED);
    let (second, two) = fixture.client_with_window("two", 100, 100, BLUE);
    let is_active = |client: &Client| {
        client.log(|log| log.configures.last().is_some_and(|(_, _, states)| states.contains(&STATE_ACTIVATED)))
    };

    fixture.compositor.activate(one.id);
    fixture.wait_for_toplevel("one", |t| t.activated);
    fixture.wait("the first client to be told", || is_active(&first));
    assert!(!is_active(&second));
    assert!(first.log(|log| log.keyboard_focus));

    fixture.compositor.activate(two.id);
    fixture.wait_for_toplevel("two", |t| t.activated);
    fixture.wait_for_toplevel("one", |t| !t.activated);
    fixture.wait("the clients to be told", || is_active(&second) && !is_active(&first));
    fixture.wait("the keyboard focus to move", || {
        second.log(|log| log.keyboard_focus) && !first.log(|log| log.keyboard_focus)
    });

    // When the active window goes away, the one used before is active again.
    second.run(|state, _| state.close());
    fixture.wait_for_toplevel("one", |t| t.activated);
}

fn compositor_sizes_and_closes_windows() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    let compositor = &fixture.compositor;

    compositor.set_fullscreen(toplevel.id, true);
    compositor.resize(toplevel.id, 300.0, 250.0);
    let toplevel = fixture.wait_for_toplevel("one", |t| t.width == 300.0);
    assert_eq!(toplevel.height, 250.0);
    assert!(client.log(|log| log.configures.last().unwrap().2.contains(&STATE_FULLSCREEN)));

    // No size leaves it to the client again.
    compositor.set_fullscreen(toplevel.id, false);
    compositor.resize(toplevel.id, 0.0, 0.0);
    fixture.wait("the size to be left to the client", || {
        client
            .log(|log| log.configures.last().is_some_and(|c| (c.0, c.1) == (0, 0) && !c.2.contains(&STATE_FULLSCREEN)))
    });

    compositor.close(toplevel.id);
    fixture.wait("the client to be asked to close", || client.log(|log| log.close_requested));
}

fn client_asks_to_be_moved() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    let compositor = &fixture.compositor;

    compositor.pointer_motion(toplevel.id, 10.0, 10.0);
    compositor.pointer_button(BTN_LEFT, true);
    fixture.wait("the button", || client.log(|log| log.pointer.len() == 2));
    client.run(|state, _| state.start_move(0));
    fixture.wait_for_toplevel("one", |t| t.moving);
    // The UI drags the window now, and the client is not told about the pointer.
    fixture.wait("the pointer to leave", || client.log(|log| log.pointer.last() == Some(&PointerEvent::Leave)));
    compositor.pointer_motion(toplevel.id, 50.0, 50.0);

    compositor.pointer_button(BTN_LEFT, false);
    fixture.wait_for_toplevel("one", |t| !t.moving);
    fixture.settle();
    assert_eq!(client.log(|log| log.pointer.last().cloned()), Some(PointerEvent::Leave));

    // A move that no button press led to is ignored.
    client.run(|state, _| state.start_move(0));
    fixture.settle();
    assert!(!fixture.toplevel("one").unwrap().moving);
}

fn client_asks_to_be_resized() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    let compositor = &fixture.compositor;
    let id = toplevel.id;
    let has_state = |state: u32| client.log(|log| log.configures.last().is_some_and(|c| c.2.contains(&state)));

    // Without a button being down there is nothing to resize with.
    client.run(|state, _| state.start_resize(ResizeEdge::BottomRight));
    fixture.settle();
    assert!(!fixture.toplevel("one").unwrap().resizing);

    compositor.pointer_motion(id, 190.0, 90.0);
    compositor.pointer_button(BTN_LEFT, true);
    fixture.wait("the button", || client.log(|log| log.pointer.len() == 2));
    client.run(|state, _| state.start_resize(ResizeEdge::BottomRight));
    let toplevel = fixture.wait_for_toplevel("one", |t| t.resizing);
    assert_eq!(toplevel.resize_edges, 10);
    fixture.wait("the pointer to leave", || client.log(|log| log.pointer.last() == Some(&PointerEvent::Leave)));

    compositor.resize_drag(id, 20.0, 10.0);
    compositor.resize_drag(id, 50.0, 30.0);
    let toplevel = fixture.wait_for_toplevel("one", |t| t.width == 250.0);
    assert_eq!(toplevel.height, 130.0);
    assert!(has_state(STATE_RESIZING));

    compositor.pointer_button(BTN_LEFT, false);
    fixture.wait_for_toplevel("one", |t| !t.resizing && t.resize_edges == 0);
    fixture.wait("the client to be told that it is over", || !has_state(STATE_RESIZING));
    compositor.resize_drag(id, 300.0, 300.0);
    fixture.settle();
    assert_eq!(fixture.toplevel("one").unwrap().width, 250.0);

    // By the top left corner, the window grows as the pointer goes up and left. The edges
    // stay known until the client has drawn in the last size.
    compositor.pointer_button(BTN_LEFT, true);
    client.run(|state, _| state.start_resize(ResizeEdge::TopLeft));
    fixture.wait_for_toplevel("one", |t| t.resizing && t.resize_edges == 5);
    compositor.resize_drag(id, -20.0, -10.0);
    compositor.pointer_button(BTN_LEFT, false);
    let toplevel = fixture.toplevel("one").unwrap();
    assert_eq!((toplevel.resizing, toplevel.resize_edges, toplevel.width), (false, 5, 250.0));
    let toplevel = fixture.wait_for_toplevel("one", |t| t.resize_edges == 0);
    assert_eq!((toplevel.width, toplevel.height), (270.0, 140.0));
}

fn client_asks_to_be_maximized() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    let compositor = &fixture.compositor;
    let has_state = |state: u32| client.log(|log| log.configures.last().is_some_and(|c| c.2.contains(&state)));

    // The UI is told, and answers with the size that goes with it.
    client.run(|state, _| state.toplevel().set_maximized());
    fixture.wait_for_toplevel("one", |t| t.maximized);
    compositor.resize(toplevel.id, 640.0, 480.0);
    let sized = fixture.wait_for_toplevel("one", |t| t.width == 640.0);
    assert_eq!(sized.height, 480.0);
    assert!(has_state(STATE_MAXIMIZED));

    // Without an answer the client is told all the same.
    client.run(|state, _| state.toplevel().unset_maximized());
    fixture.wait_for_toplevel("one", |t| !t.maximized);
    fixture.wait("the client to be told", || !has_state(STATE_MAXIMIZED));

    client.run(|state, _| state.toplevel().set_fullscreen(None));
    fixture.wait_for_toplevel("one", |t| t.fullscreen);
    fixture.wait("the client to be told", || has_state(STATE_FULLSCREEN));
    client.run(|state, _| state.toplevel().unset_fullscreen());
    fixture.wait_for_toplevel("one", |t| !t.fullscreen);

    // The UI can do the same on its own.
    compositor.set_maximized(toplevel.id, true);
    fixture.wait_for_toplevel("one", |t| t.maximized);
    fixture.wait("the client to be told", || has_state(STATE_MAXIMIZED));

    // A window that is put aside is not the one keys go to anymore.
    compositor.activate(toplevel.id);
    fixture.wait("the keyboard focus", || client.log(|log| log.keyboard_focus));
    client.run(|state, _| state.toplevel().set_minimized());
    fixture.wait_for_toplevel("one", |t| t.minimized && !t.activated);
    fixture.wait("the keyboard focus to go", || !client.log(|log| log.keyboard_focus));
    compositor.activate(toplevel.id);
    fixture.wait_for_toplevel("one", |t| !t.minimized && t.activated);
    fixture.wait("the keyboard focus", || client.log(|log| log.keyboard_focus));
}

fn client_sets_the_cursor() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    fixture.compositor.pointer_motion(toplevel.id, 10.0, 10.0);
    fixture.wait("the pointer to enter", || client.log(|log| !log.pointer.is_empty()));

    client.run(|state, qh| state.set_cursor(qh, Some((16, BLUE, 3))));
    fixture.wait("the cursor image", || matches!(&*fixture.cursor.borrow(), Some(Cursor::Image { .. })));
    let Some(Cursor::Image { image, hotspot_x, hotspot_y, width, height }) = fixture.cursor.borrow().clone() else {
        unreachable!()
    };
    assert_eq!((hotspot_x, hotspot_y, width, height), (3.0, 3.0, 16.0, 16.0));
    let pixel = image.to_rgba8_premultiplied().unwrap().as_slice()[0];
    assert_eq!((pixel.r, pixel.g, pixel.b, pixel.a), (0, 0, 255, 255));

    client.run(|state, qh| state.set_cursor(qh, None));
    fixture.wait("the cursor to be hidden", || matches!(&*fixture.cursor.borrow(), Some(Cursor::Hidden)));

    // Away from the client, the pointer is nothing special anymore.
    fixture.compositor.pointer_leave();
    fixture.wait("the usual cursor", || matches!(&*fixture.cursor.borrow(), Some(Cursor::Named("default"))));
}

fn clients_draw_for_the_scale_of_the_output() {
    let fixture = Fixture::new();
    fixture.compositor.set_output_scale(2.0);
    let client = fixture.client();
    fixture.wait("the scale", || client.log(|log| log.output_scale == 2));

    // Twice the pixels are still the same size.
    client.run(|state, qh| {
        state.buffer_scale = 2;
        state.show(qh, "one", 200, 100, RED);
    });
    let toplevel = fixture.wait_for_toplevel("one", |t| !t.views.is_empty());
    assert_eq!((toplevel.width, toplevel.height), (200.0, 100.0));
    let view = &toplevel.views[0];
    assert_eq!((view.width, view.height), (200.0, 100.0));
    let size = view.image.size();
    assert_eq!((size.width, size.height), (400, 200));

    fixture.compositor.set_output_scale(1.0);
    fixture.wait("the scale", || client.log(|log| log.output_scale == 1));
}

fn clipboard_goes_from_client_to_client() {
    let fixture = Fixture::new();
    let (first, one) = fixture.client_with_window("one", 100, 100, RED);
    let (second, two) = fixture.client_with_window("two", 100, 100, BLUE);

    // Copying is for the client that has the keyboard.
    fixture.compositor.activate(one.id);
    fixture.wait("the keyboard focus", || first.log(|log| log.keyboard_focus));
    first.run(|state, qh| state.copy(qh, "from one"));
    fixture.settle();

    // The selection is offered to whoever gets the keyboard next.
    fixture.compositor.activate(two.id);
    fixture.wait("the text to arrive", || second.log(|log| !log.clipboard.is_empty()));
    assert_eq!(second.log(|log| log.clipboard.clone()), ["from one"]);
}

fn primary_selection_goes_from_client_to_client() {
    let fixture = Fixture::new();
    let (first, one) = fixture.client_with_window("one", 100, 100, RED);
    let (second, two) = fixture.client_with_window("two", 100, 100, BLUE);

    fixture.compositor.activate(one.id);
    fixture.wait("the keyboard focus", || first.log(|log| log.keyboard_focus));
    first.run(|state, qh| state.select(qh, "selected in one"));
    fixture.settle();

    fixture.compositor.activate(two.id);
    fixture.wait("the text to arrive", || second.log(|log| !log.primary.is_empty()));
    assert_eq!(second.log(|log| log.primary.clone()), ["selected in one"]);
    // It is not the clipboard.
    assert!(second.log(|log| log.clipboard.is_empty()));
}

fn text_is_dragged_from_window_to_window() {
    let fixture = Fixture::new();
    let (source, one) = fixture.client_with_window("one", 200, 100, RED);
    let (target, two) = fixture.client_with_window("two", 200, 100, BLUE);
    let compositor = &fixture.compositor;

    compositor.pointer_motion(one.id, 10.0, 10.0);
    compositor.pointer_button(BTN_LEFT, true);
    fixture.wait("the button", || source.log(|log| log.pointer.len() == 2));
    source.run(|state, qh| state.start_drag(qh, "dragged"));

    // Over its own window first, where it could be dropped as well.
    // The pointer is the drag's while it lasts.
    fixture.wait("the drag to begin", || source.log(|log| log.pointer.last() == Some(&PointerEvent::Leave)));
    compositor.pointer_motion(one.id, 30.0, 40.0);
    compositor.pointer_motion(one.id, 35.0, 45.0);
    fixture.wait("the drag over the source", || source.log(|log| log.drag.len() == 2));
    assert_eq!(source.log(|log| log.drag.clone()), ["enter 30 40", "motion 35 45"]);

    // A UI keeps reporting the pointer to the window the button went down in. With the
    // items at these places, 350 60 in the first window is 50 60 in the second.
    compositor.set_item_position(one.id, 0.0, 0.0);
    compositor.set_item_position(two.id, 300.0, 0.0);
    compositor.pointer_motion(one.id, 350.0, 60.0);
    fixture.wait("the drag to change windows", || source.log(|log| log.drag.last().is_some_and(|d| d == "leave")));
    fixture.wait("the drag over the target", || target.log(|log| !log.drag.is_empty()));
    assert_eq!(target.log(|log| log.drag.clone()), ["enter 50 60"]);
    // The target answers the enter by saying that it takes the text. A drop before the
    // compositor has heard that is a drag that was cancelled.
    fixture.settle();

    compositor.pointer_button(BTN_LEFT, false);
    fixture.wait("the drop", || target.log(|log| !log.dropped.is_empty()));
    assert_eq!(target.log(|log| log.dropped.clone()), ["dragged"]);
    // The drag is over for the target once it got the drop.
    fixture.wait("the drag to end", || target.log(|log| log.drag.last().is_some_and(|d| d == "leave")));
    assert_eq!(target.log(|log| log.drag.clone()), ["enter 50 60", "drop", "leave"]);
}

fn keys_are_released_when_the_keyboard_goes_elsewhere() {
    let fixture = Fixture::new();
    let (first, one) = fixture.client_with_window("one", 100, 100, RED);
    let (second, two) = fixture.client_with_window("two", 100, 100, BLUE);
    let compositor = &fixture.compositor;

    compositor.activate(one.id);
    fixture.wait("the keyboard focus", || first.log(|log| log.keyboard_focus));
    compositor.key("a", true);
    // Another window becomes the active one while the key is down.
    compositor.activate(two.id);
    fixture.wait("the keyboard focus to move", || second.log(|log| log.keyboard_focus));
    // The two clients learn about it independently of each other.
    fixture.wait("the key to be released", || first.log(|log| log.keys.len() == 2));
    assert_eq!(first.log(|log| log.keys.clone()), [(KEY_A, true), (KEY_A, false)]);
    compositor.key("a", false);

    // The same when the UI takes the keyboard for itself.
    compositor.key("a", true);
    compositor.release_keys();
    compositor.key("a", false);
    fixture.wait("the keys", || second.log(|log| log.keys.len() == 2));
    fixture.settle();
    assert_eq!(second.log(|log| log.keys.clone()), [(KEY_A, true), (KEY_A, false)]);
}

fn activating_a_window_that_is_gone_changes_nothing() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 100, 100, RED);
    let compositor = &fixture.compositor;
    compositor.activate(toplevel.id);
    fixture.wait("the keyboard focus", || client.log(|log| log.keyboard_focus));

    // What a UI does when it acts on a toplevel that went away in the meantime.
    compositor.activate(toplevel.id + 1000);
    fixture.settle();
    assert!(fixture.toplevel("one").unwrap().activated);
    assert!(client.log(|log| log.keyboard_focus));
    compositor.key("a", true);
    compositor.key("a", false);
    fixture.wait("the keys", || client.log(|log| log.keys.len() == 2));
}

fn last_window_takes_the_keyboard_with_it() {
    let fixture = Fixture::new();
    let (first, one) = fixture.client_with_window("one", 100, 100, RED);
    let compositor = &fixture.compositor;
    compositor.activate(one.id);
    fixture.wait("the keyboard focus", || first.log(|log| log.keyboard_focus));

    // The only window goes away while a key is down.
    compositor.key("a", true);
    fixture.wait("the key", || first.log(|log| log.keys.len() == 1));
    first.run(|state, _| state.close());
    fixture.wait("the toplevel to be removed", || fixture.toplevel("one").is_none());

    // The key is not down for whoever comes next: it can be pressed.
    let (second, two) = fixture.client_with_window("two", 100, 100, BLUE);
    fixture.settle();
    assert!(!second.log(|log| log.keyboard_focus));
    compositor.activate(two.id);
    fixture.wait("the keyboard focus", || second.log(|log| log.keyboard_focus));
    compositor.key("a", true);
    compositor.key("a", false);
    fixture.wait("the keys", || second.log(|log| log.keys.len() == 2));
    assert_eq!(second.log(|log| log.keys.clone()), [(KEY_A, true), (KEY_A, false)]);
}

fn button_reported_twice_is_down_once() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    let compositor = &fixture.compositor;

    compositor.pointer_motion(toplevel.id, 10.0, 10.0);
    compositor.pointer_button(BTN_LEFT, true);
    compositor.pointer_button(BTN_LEFT, true);
    fixture.wait("the button", || client.log(|log| log.pointer.len() >= 2));
    client.run(|state, _| state.start_move(0));
    fixture.wait_for_toplevel("one", |t| t.moving);

    // One release ends the move, and a second one is not passed on.
    compositor.pointer_button(BTN_LEFT, false);
    fixture.wait_for_toplevel("one", |t| !t.moving);
    compositor.pointer_motion(toplevel.id, 20.0, 20.0);
    compositor.pointer_button(BTN_LEFT, false);
    fixture.settle();
    let buttons: Vec<_> =
        client.log(|log| log.pointer.iter().filter(|e| matches!(e, PointerEvent::Button(..))).cloned().collect());
    assert_eq!(buttons, [PointerEvent::Button(BTN_LEFT, true)]);
}

fn client_with_an_impossible_buffer_is_disconnected() {
    let fixture = Fixture::new();
    let (bad, _) = fixture.client_with_window("bad", 200, 100, RED);
    let (good, toplevel) = fixture.client_with_window("good", 200, 100, BLUE);

    // Rows that are shorter than the width needs never make it to a buffer: the client
    // is gone, and its window with it.
    bad.run(|state, qh| state.paint_with_short_rows(qh, 200, 100));
    fixture.wait("the toplevel to be removed", || fixture.toplevel("bad").is_none());

    // Everybody else is served as before.
    fixture.compositor.pointer_motion(toplevel.id, 10.0, 10.0);
    fixture.wait("the pointer to enter", || good.log(|log| !log.pointer.is_empty()));
    good.run(|state, qh| state.paint(qh, 120, 80, RED));
    fixture.wait_for_toplevel("good", |t| t.width == 120.0);
}

fn subsurface_is_a_view_of_its_window() {
    let fixture = Fixture::new();
    let (client, _) = fixture.client_with_window("one", 200, 100, RED);

    client.run(|state, qh| state.add_subsurface(qh, 30, 40, 50, 20, BLUE));
    let toplevel = fixture.wait_for_toplevel("one", |t| t.views.len() == 2);
    // On top of the window, where the client put it, and not a popup.
    let view = &toplevel.views[1];
    assert_eq!((view.x, view.y, view.width, view.height, view.popup), (30.0, 40.0, 50.0, 20.0, false));
    let pixel = view.image.to_rgba8_premultiplied().unwrap().as_slice()[0];
    assert_eq!((pixel.r, pixel.g, pixel.b), (0, 0, 255));
    assert_eq!((toplevel.width, toplevel.height), (200.0, 100.0));
}

fn popup_is_a_view_that_takes_the_pointer() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);

    // A menu that sticks out of the window at the bottom.
    client.run(|state, qh| state.open_popup(qh, 150, 80, 100, 60, BLUE));
    let with_popup = fixture.wait_for_toplevel("one", |t| t.views.len() == 2);
    let view = &with_popup.views[1];
    assert_eq!((view.x, view.y, view.width, view.height, view.popup), (150.0, 80.0, 100.0, 60.0, true));
    // The window is as large as it was, the area that takes the pointer covers both.
    assert_eq!((with_popup.width, with_popup.height), (200.0, 100.0));
    assert_eq!(
        (with_popup.input_x, with_popup.input_y, with_popup.input_width, with_popup.input_height),
        (0.0, 0.0, 250.0, 140.0)
    );

    // Outside of the window, on the popup: positions are relative to the popup.
    fixture.compositor.pointer_motion(toplevel.id, 220.0, 120.0);
    fixture.wait("the pointer to enter", || client.log(|log| !log.pointer.is_empty()));
    assert_eq!(client.log(|log| log.pointer.clone()), [PointerEvent::Enter(70.0, 40.0)]);
}

fn decoration_follows_what_was_agreed() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    // A client that does not know the protocol decorates itself.
    assert!(!toplevel.server_decorated);

    // Asked who is to decorate, the compositor says it is the UI.
    client.run(|state, qh| state.set_decoration(qh, None));
    fixture.wait_for_toplevel("one", |t| t.server_decorated);

    // The client can insist on either, while its window is shown.
    client.run(|state, qh| state.set_decoration(qh, Some(true)));
    fixture.wait_for_toplevel("one", |t| !t.server_decorated);
    client.run(|state, qh| state.set_decoration(qh, Some(false)));
    fixture.wait_for_toplevel("one", |t| t.server_decorated);
}

fn buffer_that_is_too_large_is_not_shown() {
    let fixture = Fixture::new();
    let (client, _) = fixture.client_with_window("one", 200, 100, RED);

    // Wider than anything that gets copied. The client may have it, it is just not shown,
    // and the UI is told why, once.
    let id = fixture.toplevel("one").unwrap().id;
    client.run(|state, qh| state.paint(qh, 20000, 4, RED));
    fixture.wait("the toplevel to be removed", || fixture.toplevel("one").is_none());
    client.run(|state, qh| state.paint(qh, 20000, 4, BLUE));
    fixture.settle();
    assert_eq!(*fixture.refusals.borrow(), [BufferRefusal { toplevel: Some(id), reason: RefusalReason::TooLarge }]);

    client.run(|state, qh| state.paint(qh, 200, 100, BLUE));
    let toplevel = fixture.wait_for_toplevel("one", |t| !t.views.is_empty());
    assert_eq!(color_of(&toplevel), BLUE);
}

fn shared_memory_of_all_clients_is_limited() {
    let fixture = Fixture::new();
    // Room for one window of 100x100 with 4 bytes per pixel, and a bit.
    fixture.compositor.set_shm_memory_limit(50_000);
    let (first, _) = fixture.client_with_window("one", 100, 100, RED);

    // Each buffer is small enough on its own, together they are too much: the second
    // window is not shown, the first one is not touched, and the UI is told.
    let second = fixture.client();
    second.run(|state, qh| state.show(qh, "two", 100, 100, BLUE));
    fixture.wait("the refusal", || !fixture.refusals.borrow().is_empty());
    assert_eq!(fixture.refusals.borrow()[0].reason, RefusalReason::MemoryLimit);
    fixture.settle();
    assert!(fixture.toplevel("two").is_none());
    assert_eq!(color_of(&fixture.toplevel("one").unwrap()), RED);

    // Redrawing in the size it has replaces what a surface holds, which always fits.
    first.run(|state, qh| state.paint(qh, 100, 100, BLUE));
    fixture.wait_for_toplevel("one", |t| color_of(t) == BLUE);

    // When the first window is gone, there is room for the second, which shows up
    // without its client doing anything.
    first.run(|state, _| state.close());
    fixture.wait("the toplevel to be removed", || fixture.toplevel("one").is_none());
    fixture.wait_for_toplevel("two", |t| !t.views.is_empty());
}

fn limit_of_shared_memory_can_be_changed() {
    let fixture = Fixture::new();
    let (_small, _) = fixture.client_with_window("small", 50, 50, RED);
    let (_large, _) = fixture.client_with_window("large", 100, 100, BLUE);
    let compositor = &fixture.compositor;

    // With room for the small window only, the large one goes.
    compositor.set_shm_memory_limit(20_000);
    fixture.wait("the large window to go", || fixture.toplevel("large").is_none());
    assert!(fixture.toplevel("small").is_some());
    assert_eq!(fixture.refusals.borrow().last().map(|r| r.reason), Some(RefusalReason::MemoryLimit));

    // With no room at all, nothing is shown.
    compositor.set_shm_memory_limit(0);
    fixture.wait("the small window to go", || fixture.toplevel("small").is_none());

    // With room again, both come back as they were, without their clients drawing.
    compositor.set_shm_memory_limit(1 << 20);
    let large = fixture.wait_for_toplevel("large", |t| !t.views.is_empty());
    let small = fixture.wait_for_toplevel("small", |t| !t.views.is_empty());
    assert_eq!((color_of(&large), color_of(&small)), (BLUE, RED));
}

fn sizes_that_are_no_sizes_do_no_harm() {
    let fixture = Fixture::new();
    let (client, toplevel) = fixture.client_with_window("one", 200, 100, RED);
    let compositor = &fixture.compositor;
    // The client must not try to draw what it is asked for here: wait until it knows.
    client.run(|state, _| state.follow_size = false);
    fixture.settle();
    let asked = || client.log(|log| log.configures.last().map(|c| (c.0, c.1)));

    // Larger than any screen is as large as it gets.
    compositor.resize(toplevel.id, 1.0e30, 1.0e30);
    fixture.wait("the largest size", || asked() == Some((65535, 65535)));
    // What is not a finite number is no size, which leaves it to the client.
    compositor.resize(toplevel.id, f32::NAN, 100.0);
    fixture.wait("no size", || asked() == Some((0, 0)));
    compositor.resize(toplevel.id, 300.0, 200.0);
    fixture.wait("a size", || asked() == Some((300, 200)));
    compositor.resize(toplevel.id, 100.0, f32::INFINITY);
    fixture.wait("no size", || asked() == Some((0, 0)));

    // The same for the distance of a resize, which is added to the size.
    compositor.pointer_motion(toplevel.id, 190.0, 90.0);
    compositor.pointer_motion(toplevel.id, f32::NAN, f32::INFINITY);
    compositor.pointer_button(BTN_LEFT, true);
    fixture.wait("the button", || client.log(|log| log.pointer.len() == 2));
    client.run(|state, _| state.start_resize(ResizeEdge::BottomRight));
    fixture.wait_for_toplevel("one", |t| t.resizing);
    compositor.resize_drag(toplevel.id, 1.0e30, f32::NAN);
    fixture.wait("the largest width", || asked() == Some((65535, 100)));
    compositor.resize_drag(toplevel.id, -1.0e30, -1.0e30);
    fixture.wait("the smallest size", || asked() == Some((1, 1)));
    compositor.resize_drag(toplevel.id, f32::INFINITY, f32::NEG_INFINITY);
    fixture.wait("the size it had", || asked() == Some((200, 100)));
    compositor.pointer_button(BTN_LEFT, false);
    compositor.pointer_axis(f32::NAN, 60.0);
    fixture.wait_for_toplevel("one", |t| !t.resizing);
}

fn programs_are_started_without_a_shell() {
    let fixture = Fixture::new();
    // What a shell would run as two commands is one argument here.
    let pid = fixture.compositor.spawn_program("true", ["; exit 1"]).expect("true exists");
    assert!(pid > 0);
    assert!(fixture.compositor.spawn_program("/nonexistent/program", [""; 0]).is_err());
}

const TESTS: &[(&str, fn())] = &[
    ("window_appears_with_its_content", window_appears_with_its_content),
    ("window_follows_what_the_client_draws", window_follows_what_the_client_draws),
    ("client_gets_frame_callbacks", client_gets_frame_callbacks),
    ("window_goes_away", window_goes_away),
    ("pointer_reaches_the_client", pointer_reaches_the_client),
    ("pointer_outside_of_the_window_is_not_on_it", pointer_outside_of_the_window_is_not_on_it),
    ("keys_reach_the_active_window", keys_reach_the_active_window),
    ("shift_is_added_for_text_that_needs_it", shift_is_added_for_text_that_needs_it),
    ("only_one_window_is_active", only_one_window_is_active),
    ("compositor_sizes_and_closes_windows", compositor_sizes_and_closes_windows),
    ("client_asks_to_be_moved", client_asks_to_be_moved),
    ("client_asks_to_be_resized", client_asks_to_be_resized),
    ("client_asks_to_be_maximized", client_asks_to_be_maximized),
    ("client_sets_the_cursor", client_sets_the_cursor),
    ("clients_draw_for_the_scale_of_the_output", clients_draw_for_the_scale_of_the_output),
    ("clipboard_goes_from_client_to_client", clipboard_goes_from_client_to_client),
    ("primary_selection_goes_from_client_to_client", primary_selection_goes_from_client_to_client),
    ("text_is_dragged_from_window_to_window", text_is_dragged_from_window_to_window),
    ("keys_are_released_when_the_keyboard_goes_elsewhere", keys_are_released_when_the_keyboard_goes_elsewhere),
    ("activating_a_window_that_is_gone_changes_nothing", activating_a_window_that_is_gone_changes_nothing),
    ("last_window_takes_the_keyboard_with_it", last_window_takes_the_keyboard_with_it),
    ("button_reported_twice_is_down_once", button_reported_twice_is_down_once),
    ("client_with_an_impossible_buffer_is_disconnected", client_with_an_impossible_buffer_is_disconnected),
    ("subsurface_is_a_view_of_its_window", subsurface_is_a_view_of_its_window),
    ("popup_is_a_view_that_takes_the_pointer", popup_is_a_view_that_takes_the_pointer),
    ("decoration_follows_what_was_agreed", decoration_follows_what_was_agreed),
    ("buffer_that_is_too_large_is_not_shown", buffer_that_is_too_large_is_not_shown),
    ("shared_memory_of_all_clients_is_limited", shared_memory_of_all_clients_is_limited),
    ("limit_of_shared_memory_can_be_changed", limit_of_shared_memory_can_be_changed),
    ("sizes_that_are_no_sizes_do_no_harm", sizes_that_are_no_sizes_do_no_harm),
    ("programs_are_started_without_a_shell", programs_are_started_without_a_shell),
];

fn main() {
    // Sockets of the tests do not belong next to those of a session.
    let runtime_dir = std::env::temp_dir().join(format!("slint-wayland-compositor-test-{}", std::process::id()));
    std::fs::create_dir_all(&runtime_dir).unwrap();
    // SAFETY: nothing else runs yet.
    unsafe { std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir) };
    i_slint_backend_testing::init_integration_test_with_system_time();

    let filter: Vec<String> = std::env::args().skip(1).filter(|argument| !argument.starts_with('-')).collect();
    let mut failed = Vec::new();
    let mut count = 0;
    for (name, test) in TESTS {
        if !filter.is_empty() && !filter.iter().any(|part| name.contains(part.as_str())) {
            continue;
        }
        count += 1;
        match std::panic::catch_unwind(test) {
            Ok(()) => println!("test {name} ... ok"),
            Err(_) => {
                println!("test {name} ... FAILED");
                failed.push(*name);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&runtime_dir);

    println!("\n{} passed, {} failed", count - failed.len(), failed.len());
    if !failed.is_empty() {
        println!("failed: {}", failed.join(", "));
        std::process::exit(1);
    }
}
