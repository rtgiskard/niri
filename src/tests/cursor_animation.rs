use std::os::fd::AsFd as _;
use std::os::unix::fs::FileExt as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use niri_config::Config;
use smithay::backend::input::{InputEvent, InputTime};
use smithay::input::pointer::CursorImageStatus;
use smithay::reexports::rustix::fs::{ftruncate, memfd_create, MemfdFlags};
use smithay::reexports::wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use smithay::reexports::wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
};
use smithay::reexports::wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_cursor_session_v1::{self, ExtImageCopyCaptureCursorSessionV1},
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use smithay::utils::Point;
use wayland_client::protocol::{
    wl_pointer::{self, WlPointer},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
};
use wayland_client::{Connection, Dispatch, QueueHandle};

use super::client::{ClientId, State as ClientState};
use super::test_input_backend::{TestInputBackend, TestPointerAxisEvent};
use super::Fixture;
use crate::backend::{Backend, Headless};
use crate::cursor::RenderCursor;
use crate::niri::{PointerVisibility, RedrawState};

wayland_client::delegate_noop!(ClientState: ignore ExtImageCaptureSourceV1);
wayland_client::delegate_noop!(ClientState: ignore ExtForeignToplevelHandleV1);
wayland_client::delegate_noop!(ClientState: ignore ExtForeignToplevelImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(ClientState: ignore ExtOutputImageCaptureSourceManagerV1);
wayland_client::delegate_noop!(ClientState: ignore ExtImageCopyCaptureManagerV1);
wayland_client::delegate_noop!(ClientState: ignore ExtImageCopyCaptureCursorSessionV1);
wayland_client::delegate_noop!(ClientState: ignore ExtImageCopyCaptureSessionV1);
wayland_client::delegate_noop!(ClientState: ignore ExtImageCopyCaptureFrameV1);
wayland_client::delegate_noop!(ClientState: ignore WlPointer);
wayland_client::delegate_noop!(ClientState: ignore WlShm);
wayland_client::delegate_noop!(ClientState: ignore WlShmPool);

type Toplevels = Arc<Mutex<Vec<ExtForeignToplevelHandleV1>>>;

impl Dispatch<ExtForeignToplevelListV1, Toplevels> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        toplevels: &Toplevels,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            toplevels.lock().unwrap().push(toplevel);
        }
    }

    wayland_client::event_created_child!(ClientState, ExtForeignToplevelListV1, [
        0 => (ExtForeignToplevelHandleV1, ())
    ]);
}

type FrameResults = Arc<Mutex<Vec<bool>>>;

impl Dispatch<ExtImageCopyCaptureFrameV1, FrameResults> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        results: &FrameResults,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready { .. } => {
                results.lock().unwrap().push(true)
            }
            ext_image_copy_capture_frame_v1::Event::Failed { .. } => {
                results.lock().unwrap().push(false)
            }
            _ => (),
        }
    }
}

type PointerSerial = Arc<Mutex<Option<u32>>>;

impl Dispatch<WlPointer, PointerSerial> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &WlPointer,
        event: wl_pointer::Event,
        serial: &PointerSerial,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_pointer::Event::Enter { serial: entered, .. } = event {
            *serial.lock().unwrap() = Some(entered);
        }
    }
}

type CursorHotspots = Arc<Mutex<Vec<(i32, i32)>>>;

impl Dispatch<ExtImageCopyCaptureCursorSessionV1, CursorHotspots> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureCursorSessionV1,
        event: ext_image_copy_capture_cursor_session_v1::Event,
        hotspots: &CursorHotspots,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_image_copy_capture_cursor_session_v1::Event::Hotspot { x, y } = event {
            hotspots.lock().unwrap().push((x, y));
        }
    }
}

type CaptureSizes = Arc<Mutex<Vec<(u32, u32)>>>;

impl Dispatch<ExtImageCopyCaptureSessionV1, CaptureSizes> for ClientState {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        sizes: &CaptureSizes,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_image_copy_capture_session_v1::Event::BufferSize { width, height } = event {
            sizes.lock().unwrap().push((width, height));
        }
    }
}

fn capture_cursor(f: &mut Fixture, output: &smithay::output::Output) {
    let state = f.niri_state();
    state
        .backend
        .with_primary_renderer(|renderer| {
            state.niri.render_for_image_copy_cursor_capture(
                renderer,
                output,
                Duration::ZERO,
            );
        })
        .unwrap();
}

// Session constraints now need a renderer, but the scheduling and fence tests deliberately
// leave captures pending instead of letting redraw() render and complete them. Install this
// backend only while creating the session, then restore the original renderer-less backend
// (including its output/IPC state) before requesting frames.
fn install_cursor_session_renderer(f: &mut Fixture) -> Backend {
    let mut backend = Backend::Headless(Headless::new());
    backend.headless().add_renderer().unwrap();
    std::mem::replace(&mut f.niri_state().backend, backend)
}

#[test]
fn egl_cursor_capture_publishes_hotspot_only_changes_outside_bitmap() {
    let mut f = Fixture::new();
    f.add_output(1, (100, 100));
    f.niri_state().backend.headless().add_renderer().unwrap();
    let output = f.niri_output(1);
    output.change_current_state(
        None,
        None,
        Some(smithay::output::Scale::Fractional(1.5)),
        None,
    );
    let id = f.add_client();
    f.double_roundtrip(id);
    let source = window_capture_source(&mut f, id);
    let serial = PointerSerial::default();
    let hotspots = CursorHotspots::default();
    let sizes = CaptureSizes::default();
    let cursor_fd = std::fs::File::from(
        memfd_create("niri-test-cursor-image", MemfdFlags::CLOEXEC).unwrap(),
    );
    let pixels = [0_u8, 0, 255, 255].repeat(12 * 8);
    ftruncate(&cursor_fd, pixels.len() as u64).unwrap();
    cursor_fd.write_all_at(&pixels, 0).unwrap();
    let (pointer, cursor_surface, manager, shm) = {
        let client = f.client(id);
        let registry = client.display.get_registry(&client.qh, ());
        let global = |interface| {
            client.state.globals.iter().find(|g| g.interface == interface).unwrap().name
        };
        let shm: WlShm = registry.bind(global("wl_shm"), 1, &client.qh, ());
        let manager: ExtImageCopyCaptureManagerV1 = registry.bind(
            global("ext_image_copy_capture_manager_v1"), 1, &client.qh, (),
        );
        let pointer = client.state.seats.keys().next().unwrap()
            .get_pointer(&client.qh, serial.clone());
        let cursor_surface = client.state.create_surface();
        let pool = shm.create_pool(cursor_fd.as_fd(), pixels.len() as i32, &client.qh, ());
        let buffer = pool.create_buffer(0, 12, 8, 48, wl_shm::Format::Argb8888, &client.qh, ());
        cursor_surface.attach(Some(&buffer), 0, 0);
        cursor_surface.damage(0, 0, 12, 8);
        cursor_surface.commit();
        (pointer, cursor_surface, manager, shm)
    };
    f.niri_state().move_cursor(Point::from((32., 32.)));
    f.double_roundtrip(id);
    let serial = serial.lock().unwrap().expect("pointer must enter the captured window");
    pointer.set_cursor(serial, Some(&cursor_surface), 2, 2);
    f.double_roundtrip(id);

    let (cursor_session, session) = {
        let client = f.client(id);
        let cursor_session = manager.create_pointer_cursor_session(
            &source, &pointer, &client.qh, hotspots.clone(),
        );
        let session = cursor_session.get_capture_session(&client.qh, sizes.clone());
        (cursor_session, session)
    };
    f.double_roundtrip(id);
    let results = FrameResults::default();
    let mut previous_pixels = None;
    for (index, (hotspot, expected)) in [
        ((2, 2), (3, 3)),
        ((-4, 10), (-6, 15)),
        ((3, -3), (4, -5)),
    ].into_iter().enumerate() {
        let size = *sizes.lock().unwrap().last().unwrap();
        let fd = std::fs::File::from(
            memfd_create("niri-test-cursor-frame", MemfdFlags::CLOEXEC).unwrap(),
        );
        let len = size.0 * size.1 * 4;
        ftruncate(&fd, u64::from(len)).unwrap();
        let frame = {
            let client = f.client(id);
            let pool = shm.create_pool(fd.as_fd(), len as i32, &client.qh, ());
            let buffer = pool.create_buffer(
                0, size.0 as i32, size.1 as i32, size.0 as i32 * 4,
                wl_shm::Format::Argb8888, &client.qh, (),
            );
            let frame = session.create_frame(&client.qh, results.clone());
            frame.attach_buffer(&buffer);
            frame.capture();
            frame
        };
        f.roundtrip(id);
        capture_cursor(&mut f, &output);
        f.double_roundtrip(id);
        assert_eq!(size, (18, 12));
        if index > 0 {
            assert_eq!(results.lock().unwrap().len(), index);
            assert!(f.niri().image_copy_cursor_sessions[0].pending_frame.is_some());
            pointer.set_cursor(serial, Some(&cursor_surface), hotspot.0, hotspot.1);
            f.double_roundtrip(id);
            capture_cursor(&mut f, &output);
            f.double_roundtrip(id);
        }
        assert_eq!(*results.lock().unwrap(), vec![true; index + 1]);
        assert_eq!(hotspots.lock().unwrap().last(), Some(&expected));
        let mut pixels = vec![0; len as usize];
        fd.read_exact_at(&mut pixels, 0).unwrap();
        if let Some(previous) = &previous_pixels {
            assert_eq!(&pixels, previous, "hotspot-only updates must preserve the bitmap");
        }
        assert!(pixels.chunks_exact(4).any(|pixel| pixel == [0, 0, 255, 255]));
        previous_pixels = Some(pixels);
        frame.destroy();
        f.roundtrip(id);
    }
    session.destroy();
    cursor_session.destroy();
    f.roundtrip(id);
}

#[test]
fn egl_empty_cursor_capture_keeps_fractional_nominal_size() {
    let mut config = Config::default();
    config.cursor.size = 25;
    let mut f = Fixture::with_config(config);
    f.add_output(1, (100, 100));
    f.niri_state().backend.headless().add_renderer().unwrap();
    let output = f.niri_output(1);
    output.change_current_state(
        None,
        None,
        Some(smithay::output::Scale::Fractional(1.5)),
        None,
    );
    f.niri().cursor_manager.set_cursor_image(CursorImageStatus::Hidden);
    let id = f.add_client();
    f.double_roundtrip(id);
    let sizes = CaptureSizes::default();
    let (cursor_session, session, shm) = {
        let client = f.client(id);
        let registry = client.display.get_registry(&client.qh, ());
        let global = |interface| {
            client.state.globals.iter().find(|g| g.interface == interface).unwrap().name
        };
        let source_manager: ExtOutputImageCaptureSourceManagerV1 = registry.bind(
            global("ext_output_image_capture_source_manager_v1"), 1, &client.qh, (),
        );
        let manager: ExtImageCopyCaptureManagerV1 = registry.bind(
            global("ext_image_copy_capture_manager_v1"), 1, &client.qh, (),
        );
        let shm: WlShm = registry.bind(global("wl_shm"), 1, &client.qh, ());
        let source = source_manager.create_source(
            client.state.outputs.keys().next().unwrap(), &client.qh, (),
        );
        let pointer = client.state.seats.keys().next().unwrap().get_pointer(&client.qh, ());
        let cursor_session = manager.create_pointer_cursor_session(
            &source, &pointer, &client.qh, (),
        );
        let session = cursor_session.get_capture_session(&client.qh, sizes.clone());
        (cursor_session, session, shm)
    };
    f.double_roundtrip(id);
    assert_eq!(*sizes.lock().unwrap(), [(38, 38)]);
    let len = 38 * 38 * 4;
    let fd = std::fs::File::from(
        memfd_create("niri-test-empty-cursor", MemfdFlags::CLOEXEC).unwrap(),
    );
    ftruncate(&fd, len as u64).unwrap();
    fd.write_all_at(&vec![255; len as usize], 0).unwrap();
    let results = FrameResults::default();
    let frame = {
        let client = f.client(id);
        let pool = shm.create_pool(fd.as_fd(), len, &client.qh, ());
        let buffer = pool.create_buffer(0, 38, 38, 38 * 4, wl_shm::Format::Argb8888, &client.qh, ());
        let frame = session.create_frame(&client.qh, results.clone());
        frame.attach_buffer(&buffer);
        frame.capture();
        frame
    };
    f.roundtrip(id);
    capture_cursor(&mut f, &output);
    f.double_roundtrip(id);
    assert_eq!(*results.lock().unwrap(), [true]);
    let mut pixels = vec![255; len as usize];
    fd.read_exact_at(&mut pixels, 0).unwrap();
    assert!(pixels.iter().all(|&byte| byte == 0));
    frame.destroy();
    session.destroy();
    cursor_session.destroy();
    f.roundtrip(id);
}

struct AnimatedTheme(PathBuf);

impl AnimatedTheme {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "niri-animation-output-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let cursor = path.join("cursors_scalable/default");
        std::fs::create_dir_all(&cursor).unwrap();
        std::fs::write(
            cursor.join("metadata.json"),
            r#"[{"filename":"red.svg","nominal_size":24,"hotspot_x":4,"hotspot_y":4,"delay":20},{"filename":"blue.svg","nominal_size":24,"hotspot_x":4,"hotspot_y":4,"delay":20}]"#,
        )
        .unwrap();
        for color in ["red", "blue"] {
            std::fs::write(
                cursor.join(format!("{color}.svg")),
                format!("<svg xmlns='http://www.w3.org/2000/svg' width='8' height='8'><path fill='{color}' d='M0 0H8V8H0Z'/></svg>"),
            )
            .unwrap();
        }
        Self(path)
    }

    fn fixture(&self) -> Fixture {
        let mut config = Config::default();
        config.cursor.theme = self.0.to_str().unwrap().to_owned();
        config.cursor.size = 24;
        let mut f = Fixture::with_config(config);
        f.add_output(1, (100, 100));
        f.add_output(2, (100, 100));
        // Exercise real worker preparation and completion, not an injected animation predicate.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            f.state.server.dispatch();
            if matches!(f.niri().cursor_manager.get_render_cursor(1.0), RenderCursor::Named { cursor } if cursor.is_animated_cursor())
            {
                break;
            }
            assert!(Instant::now() < deadline, "animated SVG did not load");
            std::thread::sleep(Duration::from_millis(1));
        }
        f.niri_complete_animations();
        f
    }
}

impl Drop for AnimatedTheme {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn redraw(f: &mut Fixture) {
    let state = f.niri_state();
    state.niri.queue_redraw_all();
    state.niri.redraw_queued_outputs(&mut state.backend);
}

fn animation_flags(f: &mut Fixture) -> [bool; 2] {
    let outputs = [f.niri_output(1), f.niri_output(2)];
    outputs.map(|output| f.niri().output_state[&output].unfinished_animations_remain)
}

fn tablet_at(f: &mut Fixture, output: u8, offset: (f64, f64)) {
    let output = f.niri_output(output);
    let origin = f.niri().global_space.output_geometry(&output).unwrap().loc;
    f.niri().tablet_cursor_location = Some(origin.to_f64() + Point::from(offset));
}

#[test]
fn hidden_animated_svg_does_not_keep_outputs_redrawing() {
    let theme = AnimatedTheme::new();
    let mut f = theme.fixture();
    tablet_at(&mut f, 1, (50., 50.));
    for visibility in [PointerVisibility::Hidden, PointerVisibility::Disabled] {
        f.niri().pointer_visibility = visibility;
        redraw(&mut f);
        assert_eq!(animation_flags(&mut f), [false, false]);
        assert!(matches!(
            f.niri().cursor_manager.cursor_image(),
            CursorImageStatus::Named(_)
        ));
        assert!(matches!(
            f.niri().cursor_manager.get_render_cursor(1.0),
            RenderCursor::Named { cursor } if cursor.is_animated_cursor()
        ));
    }
}

#[test]
fn wheel_reveal_restarts_idle_cursor_animation() {
    let theme = AnimatedTheme::new();
    let mut f = theme.fixture();
    f.niri_state().move_cursor(Point::from((50., 50.)));
    for (visibility, tablet) in [
        (PointerVisibility::Hidden, false),
        (PointerVisibility::Disabled, false),
        (PointerVisibility::Visible, true),
    ] {
        f.niri().pointer_visibility = visibility;
        if tablet {
            tablet_at(&mut f, 2, (50., 50.));
        }
        redraw(&mut f);
        let output = f.niri_output(1);
        assert!(matches!(
            f.niri().output_state[&output].redraw_state,
            RedrawState::Idle
        ));
        let state = f.niri_state();
        state.process_input_event(InputEvent::<TestInputBackend>::PointerAxis {
            event: TestPointerAxisEvent,
        });
        // No artificial queue_redraw_all(): the input transition must wake the output.
        state.niri.redraw_queued_outputs(&mut state.backend);
        assert_eq!(animation_flags(&mut f), [true, false]);
    }
}

#[test]
fn animated_svg_only_keeps_overlapping_outputs_redrawing() {
    let theme = AnimatedTheme::new();
    let mut f = theme.fixture();
    f.niri().pointer_visibility = PointerVisibility::Visible;
    tablet_at(&mut f, 1, (50., 50.));
    redraw(&mut f);
    assert_eq!(animation_flags(&mut f), [true, false]);

    tablet_at(&mut f, 2, (50., 50.));
    redraw(&mut f);
    assert_eq!(animation_flags(&mut f), [false, true]);

    // Both outputs abut horizontally. The hotspot lies outside output 1, but its image
    // still overlaps it. This also exercises the tablet location rather than the mouse.
    let first = f.niri_output(1);
    let second = f.niri_output(2);
    let first_geo = f.niri().global_space.output_geometry(&first).unwrap();
    let second_geo = f.niri().global_space.output_geometry(&second).unwrap();
    assert_eq!(first_geo.loc.x + first_geo.size.w, second_geo.loc.x);
    tablet_at(&mut f, 2, (2., 50.));
    redraw(&mut f);
    assert_eq!(animation_flags(&mut f), [true, true]);

    // No display consumer when the entire image is outside all outputs.
    tablet_at(&mut f, 1, (-20., -20.));
    redraw(&mut f);
    assert_eq!(animation_flags(&mut f), [false, false]);
}

#[test]
fn hidden_cursor_capture_only_animates_while_a_frame_is_pending() {
    pending_cursor_capture(false);
}

#[test]
fn hidden_window_cursor_capture_only_animates_while_a_frame_is_pending() {
    pending_cursor_capture(true);
}

fn window_capture_source(f: &mut Fixture, id: ClientId) -> ExtImageCaptureSourceV1 {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(50, 50);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();

    let toplevels = Toplevels::default();
    let client = f.client(id);
    let registry = client.display.get_registry(&client.qh, ());
    let global = client
        .state
        .globals
        .iter()
        .find(|g| g.interface == "ext_foreign_toplevel_list_v1")
        .unwrap()
        .name;
    let list: ExtForeignToplevelListV1 = registry.bind(global, 1, &client.qh, toplevels.clone());
    f.double_roundtrip(id);
    let client = f.client(id);
    let global = client
        .state
        .globals
        .iter()
        .find(|g| g.interface == "ext_foreign_toplevel_image_capture_source_manager_v1")
        .unwrap()
        .name;
    let manager: ExtForeignToplevelImageCaptureSourceManagerV1 =
        registry.bind(global, 1, &client.qh, ());
    let source = manager.create_source(&toplevels.lock().unwrap()[0], &client.qh, ());
    list.stop();
    source
}

#[test]
fn window_capture_tags_follow_content_sessions_not_cursor_sessions() {
    let mut f = Fixture::with_config(
        Config::parse_mem(
            r#"
        window-rule {
            match is-window-cast-target=true
            tag "captured"
        }
    "#,
        )
        .unwrap(),
    );
    f.add_output(1, (100, 100));
    f.niri_state().backend.headless().add_renderer().unwrap();
    let id = f.add_client();
    f.double_roundtrip(id);
    let source = window_capture_source(&mut f, id);
    let (manager, cursor_session) = {
        let client = f.client(id);
        let registry = client.display.get_registry(&client.qh, ());
        let global = client
            .state
            .globals
            .iter()
            .find(|g| g.interface == "ext_image_copy_capture_manager_v1")
            .unwrap()
            .name;
        let manager: ExtImageCopyCaptureManagerV1 = registry.bind(global, 1, &client.qh, ());
        let pointer = client
            .state
            .seats
            .keys()
            .next()
            .unwrap()
            .get_pointer(&client.qh, ());
        let cursor_session =
            manager.create_pointer_cursor_session(&source, &pointer, &client.qh, ());
        (manager, cursor_session)
    };
    let tags = |f: &mut Fixture| {
        f.niri()
            .layout
            .windows()
            .next()
            .unwrap()
            .1
            .tags()
            .cloned()
            .collect::<Vec<_>>()
    };
    f.double_roundtrip(id);
    assert!(
        tags(&mut f).is_empty(),
        "cursor-only capture is not a window content cast"
    );

    let session = {
        let client = f.client(id);
        manager.create_session(&source, Options::empty(), &client.qh, ())
    };
    f.double_roundtrip(id);
    assert!(tags(&mut f).is_empty(), "session creation is not a content cast");
    let size = f.niri().image_copy_sessions[0]
        .session
        .current_constraints()
        .unwrap()
        .size;
    let fd = memfd_create("niri-test-window-cast", MemfdFlags::CLOEXEC).unwrap();
    let len = size.w * size.h * 4;
    ftruncate(&fd, len as u64).unwrap();
    let buffer = {
        let client = f.client(id);
        let registry = client.display.get_registry(&client.qh, ());
        let global = client.state.globals.iter().find(|g| g.interface == "wl_shm")
            .unwrap().name;
        let shm: WlShm = registry.bind(global, 1, &client.qh, ());
        let pool = shm.create_pool(fd.as_fd(), len, &client.qh, ());
        pool.create_buffer(
            0, size.w, size.h, size.w * 4, wl_shm::Format::Argb8888, &client.qh, (),
        )
    };
    for request in 1..=2 {
        let frame = {
            let client = f.client(id);
            let frame = session.create_frame(&client.qh, ());
            frame.attach_buffer(&buffer);
            frame.capture();
            frame
        };
        f.double_roundtrip(id);
        if request == 1 {
            assert!(tags(&mut f).is_empty(), "one-shot screenshots are not casts");
        } else {
            assert_eq!(tags(&mut f), ["captured"]);
        }
        frame.destroy();
        f.roundtrip(id);
    }

    session.destroy();
    f.double_roundtrip(id);
    assert!(
        tags(&mut f).is_empty(),
        "content capture ending must clear derived tags even while cursor capture remains"
    );
    cursor_session.destroy();
    f.roundtrip(id);
}

#[test]
fn image_copy_fence_registration_failure_rejects_frame_and_allows_retry() {
    use smithay::backend::renderer::sync::{Fence, Interrupted, SyncPoint};
    use smithay::utils::{Rectangle, Transform};

    #[derive(Debug)]
    struct UnpollableFence;

    impl Fence for UnpollableFence {
        fn is_signaled(&self) -> bool {
            false
        }
        fn wait(&self) -> Result<(), Interrupted> {
            Err(Interrupted)
        }
        fn is_exportable(&self) -> bool {
            true
        }
        fn export(&self) -> Option<std::os::fd::OwnedFd> {
            // epoll rejects this fd, deterministically exercising insert_source failure.
            Some(std::fs::File::open("/dev/null").unwrap().into())
        }
    }

    let mut f = Fixture::with_config(Config::default());
    f.add_output(1, (100, 100));
    let id = f.add_client();
    f.double_roundtrip(id);
    let rendererless_backend = install_cursor_session_renderer(&mut f);
    let (cursor_session, session, shm) = {
        let client = f.client(id);
        let registry = client.display.get_registry(&client.qh, ());
        let global = |interface| {
            client
                .state
                .globals
                .iter()
                .find(|g| g.interface == interface)
                .unwrap()
                .name
        };
        let manager: ExtImageCopyCaptureManagerV1 = registry.bind(
            global("ext_image_copy_capture_manager_v1"),
            1,
            &client.qh,
            (),
        );
        let shm: WlShm = registry.bind(global("wl_shm"), 1, &client.qh, ());
        let source_manager: ExtOutputImageCaptureSourceManagerV1 = registry.bind(
            global("ext_output_image_capture_source_manager_v1"),
            1,
            &client.qh,
            (),
        );
        let output = client.state.outputs.keys().next().unwrap();
        let source = source_manager.create_source(output, &client.qh, ());
        let pointer = client
            .state
            .seats
            .keys()
            .next()
            .unwrap()
            .get_pointer(&client.qh, ());
        let cursor_session =
            manager.create_pointer_cursor_session(&source, &pointer, &client.qh, ());
        let session = cursor_session.get_capture_session(&client.qh, ());
        (cursor_session, session, shm)
    };
    f.double_roundtrip(id);
    f.niri_state().backend = rendererless_backend;
    let size = f.niri().image_copy_cursor_sessions[0]
        .session
        .current_constraints()
        .unwrap()
        .size;
    let len = size.w * size.h * 4;
    let fd = memfd_create("niri-test-fence-capture", MemfdFlags::CLOEXEC).unwrap();
    ftruncate(&fd, len as u64).unwrap();
    let buffer = {
        let client = f.client(id);
        let pool = shm.create_pool(fd.as_fd(), len, &client.qh, ());
        pool.create_buffer(
            0,
            size.w,
            size.h,
            size.w * 4,
            wl_shm::Format::Argb8888,
            &client.qh,
            (),
        )
    };
    for (sync, expected) in [
        (SyncPoint::from(UnpollableFence), false),
        (SyncPoint::signaled(), true),
    ] {
        let results = FrameResults::default();
        let frame = {
            let client = f.client(id);
            let frame = session.create_frame(&client.qh, results.clone());
            frame.attach_buffer(&buffer);
            frame.damage_buffer(0, 0, size.w, size.h);
            frame.capture();
            frame
        };
        f.roundtrip(id);
        let niri = f.niri();
        let pending = niri.image_copy_cursor_sessions[0]
            .pending_frame
            .take()
            .unwrap();
        let accepted = crate::handlers::image_copy_capture::frame_success_after_sync(
            pending,
            Transform::Normal,
            vec![Rectangle::from_size(size)],
            Duration::ZERO,
            sync,
            &niri.event_loop,
        );
        assert_eq!(accepted, expected);
        f.double_roundtrip(id);
        assert_eq!(*results.lock().unwrap(), [expected]);
        frame.destroy();
        f.roundtrip(id);
    }
    session.destroy();
    cursor_session.destroy();
    f.roundtrip(id);
}

fn pending_cursor_capture(window_source: bool) {
    let theme = AnimatedTheme::new();
    let mut f = theme.fixture();
    let id = f.add_client();
    f.double_roundtrip(id);
    let window_capture_source = window_source.then(|| window_capture_source(&mut f, id));
    let rendererless_backend = install_cursor_session_renderer(&mut f);
    let (cursor_session, capture_session, shm) = {
        let client = f.client(id);
        let registry = client.display.get_registry(&client.qh, ());
        let global = |interface: &str| {
            client
                .state
                .globals
                .iter()
                .find(|g| g.interface == interface)
                .unwrap()
                .name
        };
        let source_manager: ExtOutputImageCaptureSourceManagerV1 = registry.bind(
            global("ext_output_image_capture_source_manager_v1"),
            1,
            &client.qh,
            (),
        );
        let capture_manager: ExtImageCopyCaptureManagerV1 = registry.bind(
            global("ext_image_copy_capture_manager_v1"),
            1,
            &client.qh,
            (),
        );
        let shm: WlShm = registry.bind(global("wl_shm"), 1, &client.qh, ());
        let output = client
            .state
            .outputs
            .iter()
            .find(|(_, name)| *name == "headless-1")
            .unwrap()
            .0;
        let source = window_capture_source
            .unwrap_or_else(|| source_manager.create_source(output, &client.qh, ()));
        let seat = client.state.seats.keys().next().unwrap();
        let pointer = seat.get_pointer(&client.qh, ());
        let cursor_session =
            capture_manager.create_pointer_cursor_session(&source, &pointer, &client.qh, ());
        let capture_session = cursor_session.get_capture_session(&client.qh, ());
        (cursor_session, capture_session, shm)
    };
    f.roundtrip(id);
    f.niri_state().backend = rendererless_backend;
    assert_eq!(f.niri().image_copy_cursor_sessions.len(), 1);
    f.niri().pointer_visibility = PointerVisibility::Hidden;
    tablet_at(&mut f, 2, (50., 50.));
    redraw(&mut f);
    assert_eq!(animation_flags(&mut f), [false, false]);

    let size = f.niri().image_copy_cursor_sessions[0]
        .session
        .current_constraints()
        .unwrap()
        .size;
    let fd = memfd_create("niri-test-cursor-capture", MemfdFlags::CLOEXEC).unwrap();
    let len = size.w * size.h * 4;
    ftruncate(&fd, len as u64).unwrap();
    let frame = {
        let client = f.client(id);
        let pool = shm.create_pool(fd.as_fd(), len, &client.qh, ());
        let buffer = pool.create_buffer(
            0,
            size.w,
            size.h,
            size.w * 4,
            wl_shm::Format::Argb8888,
            &client.qh,
            (),
        );
        let frame = capture_session.create_frame(&client.qh, ());
        frame.attach_buffer(&buffer);
        frame.damage_buffer(0, 0, size.w, size.h);
        frame.capture();
        frame
    };
    f.roundtrip(id);
    assert!(f.niri().image_copy_cursor_sessions[0]
        .pending_frame
        .is_some());
    // Explicit image demand is scoped to its source, even when hidden and on another output.
    for visibility in [
        PointerVisibility::Hidden,
        PointerVisibility::Disabled,
        PointerVisibility::Visible,
    ] {
        f.niri().pointer_visibility = visibility;
        redraw(&mut f);
        assert_eq!(
            animation_flags(&mut f),
            [true, visibility == PointerVisibility::Visible]
        );
    }

    f.niri().pointer_visibility = PointerVisibility::Hidden;
    if window_source {
        f.niri_state().do_action(
            niri_config::Action::MoveWindowToMonitor("headless-2".into()),
            false,
            InputTime::now(),
        );
        f.double_roundtrip(id);
        f.niri_complete_animations();
        redraw(&mut f);
        assert_eq!(animation_flags(&mut f), [false, true]);
    }

    frame.destroy();
    f.roundtrip(id);
    assert!(f.niri().image_copy_cursor_sessions[0]
        .pending_frame
        .is_none());
    redraw(&mut f);
    assert_eq!(animation_flags(&mut f), [false, false]);
    capture_session.destroy();
    cursor_session.destroy();
    f.roundtrip(id);
}

#[test]
fn egl_hidden_cursor_remains_toggleable_in_screenshots() {
    let theme = AnimatedTheme::new();
    let mut f = theme.fixture();
    f.niri_state().backend.headless().add_renderer().unwrap();
    tablet_at(&mut f, 1, (50., 50.));
    let output = f.niri_output(1);
    let state = f.niri_state();
    state.niri.pointer_visibility = PointerVisibility::Hidden;
    state
        .backend
        .with_primary_renderer(|renderer| {
            let screenshots = state.niri.capture_screenshots(renderer).collect();
            assert!(state.niri.screenshot_ui.open(
                renderer,
                screenshots,
                output.clone(),
                false,
                None
            ));
            let (_, without_pointer) = state.niri.screenshot_ui.capture(renderer).unwrap();
            state.niri.screenshot_ui.toggle_pointer();
            let (_, with_pointer) = state.niri.screenshot_ui.capture(renderer).unwrap();
            assert_ne!(
                with_pointer, without_pointer,
                "hidden image must remain toggleable in screenshots"
            );
            state.niri.screenshot_ui.close();

            state.niri.pointer_visibility = PointerVisibility::Disabled;
            let screenshots = state.niri.capture_screenshots(renderer).collect();
            assert!(state
                .niri
                .screenshot_ui
                .open(renderer, screenshots, output, false, None));
            let (_, without_pointer) = state.niri.screenshot_ui.capture(renderer).unwrap();
            state.niri.screenshot_ui.toggle_pointer();
            let (_, with_pointer) = state.niri.screenshot_ui.capture(renderer).unwrap();
            assert_eq!(
                with_pointer, without_pointer,
                "disabled pointer must stay absent from screenshots"
            );
        })
        .unwrap();
}
