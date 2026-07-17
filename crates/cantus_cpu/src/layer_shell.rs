use crate::{
    CantusApp,
    config::{Layer as ConfigLayer, LayerAnchor as ConfigLayerAnchor},
    model::Rect,
};
use glam::vec2;
use std::{
    collections::hash_map::DefaultHasher,
    ffi::c_void,
    hash::{Hash, Hasher},
    ptr::NonNull,
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_callback::{self, WlCallback},
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_region::WlRegion,
        wl_registry::{self, WlRegistry},
        wl_seat::{self, WlSeat},
        wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::{
    fractional_scale::v1::client::{
        wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        wp_fractional_scale_v1::{self, WpFractionalScaleV1},
    },
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer as LayerStyle, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor as LayerAnchor, ZwlrLayerSurfaceV1},
};
use wgpu::SurfaceTargetUnsafe;
use wgpu::rwh::{RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle};
use xkbcommon::xkb;

pub fn run() {
    let connection = Connection::connect_to_env().expect("Failed to connect to Wayland display");
    let mut event_queue = connection.new_event_queue();
    let qhandle = event_queue.handle();
    connection.display().get_registry(&qhandle, ());

    let display_ptr = NonNull::new(connection.backend().display_ptr().cast::<c_void>())
        .expect("Failed to get display pointer");
    let mut app = LayerShellApp::new(display_ptr);

    event_queue
        .roundtrip(&mut app)
        .expect("Initial roundtrip failed");
    let compositor = app.compositor.take().expect("Missing compositor");
    let layer_shell = app.layer_shell.take().expect("Missing layer shell");
    assert!(!app.outputs.is_empty(), "No Wayland outputs found");

    event_queue
        .roundtrip(&mut app)
        .expect("Failed to fetch output details");

    let wl_surface = compositor.create_surface(&qhandle, ());
    let surface_ptr = NonNull::new(wl_surface.id().as_ptr().cast::<c_void>())
        .expect("Failed to get surface pointer");
    app.surface_ptr = Some(surface_ptr);
    app.select_output();

    let surface = app.wl_surface.insert(wl_surface);
    if let (Some(vp), Some(fm)) = (app.viewporter.take(), app.fractional_manager.take()) {
        app.viewport = Some(vp.get_viewport(surface, &qhandle, ()));
        app.fractional = Some(fm.get_fractional_scale(surface, &qhandle, ()));
    }

    let layer_surface = layer_shell.get_layer_surface(
        surface,
        app.outputs.get(app.output_index).map(|info| &info.handle),
        match app.cantus.config.layer {
            ConfigLayer::Background => LayerStyle::Background,
            ConfigLayer::Bottom => LayerStyle::Bottom,
            ConfigLayer::Top => LayerStyle::Top,
            ConfigLayer::Overlay => LayerStyle::Overlay,
        },
        "cantus".into(),
        &qhandle,
        (),
    );
    let (_, total_height) = app.cantus.logical_surface_size();
    layer_surface.set_size(0, total_height as u32);
    layer_surface.set_anchor(match app.cantus.config.layer_anchor {
        ConfigLayerAnchor::Top => LayerAnchor::Top | LayerAnchor::Left | LayerAnchor::Right,
        ConfigLayerAnchor::Bottom => LayerAnchor::Bottom | LayerAnchor::Left | LayerAnchor::Right,
    });
    layer_surface.set_exclusive_zone(-1);

    surface.commit();
    connection.flush().expect("Failed to flush initial commit");

    app.compositor = Some(compositor);

    while !app.should_exit {
        event_queue
            .blocking_dispatch(&mut app)
            .expect("Wayland dispatch error");
    }
}

struct OutputInfo {
    handle: WlOutput,
    name: Option<String>,
    description: Option<String>,
    make_model: Option<String>,
}

#[derive(Default)]
struct AutoHideState {
    pointer_over_contents: bool,
    modifier_held: bool,
    action_suppressed: bool,
    hide_deadline: Option<Instant>,
}

fn bounding_rects(rects: impl Iterator<Item = Rect>) -> Option<Rect> {
    rects.reduce(|bounds, rect| {
        Rect::new(
            bounds.x0.min(rect.x0),
            bounds.y0.min(rect.y0),
            bounds.x1.max(rect.x1),
            bounds.y1.max(rect.y1),
        )
    })
}

const fn anchored_passthrough_rect(
    bounds: Rect,
    anchor: ConfigLayerAnchor,
    surface_height: f32,
) -> Rect {
    match anchor {
        ConfigLayerAnchor::Top => Rect::new(bounds.x0, 0.0, bounds.x1, bounds.y1),
        ConfigLayerAnchor::Bottom => Rect::new(bounds.x0, bounds.y0, bounds.x1, surface_height),
    }
}

impl AutoHideState {
    fn update_pointer(&mut self, over_contents: bool, now: Instant, delay: Duration) {
        if over_contents && !self.pointer_over_contents {
            self.action_suppressed = false;
            self.hide_deadline = now.checked_add(delay);
        } else if !over_contents {
            self.action_suppressed = false;
            self.hide_deadline = None;
        }
        self.pointer_over_contents = over_contents;
    }

    const fn suppress_for_action(&mut self) {
        if self.pointer_over_contents {
            self.action_suppressed = true;
        }
    }

    fn should_hide(&self, now: Instant) -> bool {
        self.pointer_over_contents
            && !self.modifier_held
            && !self.action_suppressed
            && self.hide_deadline.is_some_and(|deadline| now >= deadline)
    }
}

impl OutputInfo {
    fn matches(&self, target: &str) -> bool {
        [&self.name, &self.make_model, &self.description]
            .into_iter()
            .flatten()
            .any(|description| description.contains(target))
    }
}

pub struct LayerShellApp {
    pub cantus: CantusApp,

    is_configured: bool,
    should_exit: bool,

    compositor: Option<WlCompositor>,
    layer_shell: Option<ZwlrLayerShellV1>,
    seat: Option<WlSeat>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    keyboard_state: Option<xkb::State>,
    auto_hide: AutoHideState,
    outputs: Vec<OutputInfo>,
    output_index: usize,
    last_hitbox_hash: u64,

    surface_ptr: Option<NonNull<c_void>>,
    wl_surface: Option<WlSurface>,
    viewport: Option<WpViewport>,
    fractional: Option<WpFractionalScaleV1>,
    frame_callback: Option<WlCallback>,
    viewporter: Option<WpViewporter>,
    fractional_manager: Option<WpFractionalScaleManagerV1>,
    display_ptr: NonNull<c_void>,
}

impl LayerShellApp {
    fn new(display_ptr: NonNull<c_void>) -> Self {
        Self {
            cantus: CantusApp::default(),
            is_configured: false,
            should_exit: false,
            compositor: None,
            layer_shell: None,
            seat: None,
            pointer: None,
            keyboard: None,
            keyboard_state: None,
            auto_hide: AutoHideState::default(),
            outputs: Vec::new(),
            output_index: 0,
            last_hitbox_hash: 0,
            surface_ptr: None,
            wl_surface: None,
            viewport: None,
            fractional: None,
            frame_callback: None,
            viewporter: None,
            fractional_manager: None,
            display_ptr,
        }
    }

    fn request_frame(&mut self, qhandle: &QueueHandle<Self>) {
        if self.frame_callback.is_none()
            && let Some(surface) = &self.wl_surface
        {
            self.frame_callback = Some(surface.frame(qhandle, ()));
        }
    }

    fn update_auto_hide_request(&mut self) {
        self.cantus.render.auto_hide_requested =
            self.cantus.config.auto_hide && self.auto_hide.should_hide(Instant::now());
    }

    fn suppress_auto_hide_for_action(&mut self) {
        self.auto_hide.suppress_for_action();
        self.update_auto_hide_request();
    }

    fn update_pointer_position(&mut self, surface_pos: glam::Vec2) -> bool {
        self.cantus.render.surface_mouse_pos = surface_pos;
        self.cantus.render.uniforms.mouse_pos =
            surface_pos - self.cantus.render.uniforms.content_offset;
        let over_contents = self.cantus.pointer_over_contents(surface_pos);
        self.cantus.interaction.mouse_pressure = if over_contents { 1.0 } else { 0.0 };
        self.auto_hide.update_pointer(
            over_contents,
            Instant::now(),
            Duration::from_millis(self.cantus.config.auto_hide_delay_ms),
        );
        self.update_auto_hide_request();
        over_contents
    }

    fn ensure_surface(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 || !self.is_configured {
            return;
        }

        if let Some(gpu) = &mut self.cantus.render.gpu {
            gpu.resize_surface(width, height);
            return;
        }

        let Some(surface_ptr) = self.surface_ptr else {
            return;
        };
        let target = SurfaceTargetUnsafe::RawHandle {
            raw_display_handle: Some(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
                self.display_ptr,
            ))),
            raw_window_handle: RawWindowHandle::Wayland(WaylandWindowHandle::new(surface_ptr)),
        };
        let surface = unsafe { self.cantus.render.instance.create_surface_unsafe(target) }
            .expect("Failed to create surface");

        self.cantus.configure_render_surface(surface, width, height);
    }

    fn select_output(&mut self) {
        self.output_index = self
            .cantus
            .config
            .monitor
            .as_ref()
            .and_then(|target| self.outputs.iter().position(|info| info.matches(target)))
            .unwrap_or(0);
    }

    fn try_render_frame(&mut self, qhandle: &QueueHandle<Self>) {
        // Frame callbacks also drive the auto-hide deadline when there are no
        // pointer events between entering the bar and the timer expiring.
        self.update_auto_hide_request();
        let (buffer_width, buffer_height) = self.cantus.buffer_size();
        self.ensure_surface(buffer_width, buffer_height);

        if self.cantus.render() {
            tracing::warn!("wgpu surface was lost; recreating it");
            self.cantus.render.gpu = None;
            self.ensure_surface(buffer_width, buffer_height);
        }
        self.update_input_region(qhandle);
        self.request_frame(qhandle);
        if let Some(surface) = &self.wl_surface {
            surface.commit();
        }
    }

    fn update_scale_and_viewport(&self) {
        let (logical_width, logical_height) = self.cantus.logical_surface_size();
        let (buffer_width, buffer_height) = self.cantus.buffer_size();
        if let Some(surface) = &self.wl_surface {
            surface.set_buffer_scale(
                self.viewport
                    .as_ref()
                    .map_or_else(|| self.cantus.render.scale.ceil() as i32, |_| 1),
            );
        }
        if let Some(viewport) = &self.viewport {
            viewport.set_source(0.0, 0.0, f64::from(buffer_width), f64::from(buffer_height));
            viewport.set_destination(logical_width as i32, logical_height as i32);
        }
    }

    fn update_input_region(&mut self, qhandle: &QueueHandle<Self>) {
        let (Some(wl_surface), Some(compositor)) = (&self.wl_surface, &self.compositor) else {
            return;
        };
        let mut hasher = DefaultHasher::new();
        let offset = self.cantus.render.uniforms.content_offset.y;
        let sensor_active = self.cantus.config.auto_hide && offset.abs() >= 0.5;
        let input_bounds = self.cantus.input_bounds();
        sensor_active.hash(&mut hasher);
        for r in self.cantus.input_rects() {
            [r.x0, r.y0, r.x1, r.y1]
                .map(|value| value.round() as i32)
                .hash(&mut hasher);
            [r.x0, r.y0 + offset, r.x1, r.y1 + offset]
                .map(|value| value.round() as i32)
                .hash(&mut hasher);
        }
        let hash = hasher.finish();

        if hash != self.last_hitbox_hash {
            let region = compositor.create_region(qhandle, ());

            // Once the contents start moving, use one perimeter around the
            // whole bar. Unlike per-item rings, this has no internal edges for
            // the pointer to cross while moving through gaps between items.
            // The center remains a passthrough hole over the original bar and
            // extends toward the anchored edge, where the contents disappear.
            if sensor_active && let Some(r) = input_bounds {
                const REVEAL_SENSOR_PADDING: f32 = 12.0;
                region.add(
                    (r.x0 - REVEAL_SENSOR_PADDING).round() as i32,
                    (r.y0 - REVEAL_SENSOR_PADDING).round() as i32,
                    (r.x1 - r.x0 + REVEAL_SENSOR_PADDING * 2.0).round() as i32,
                    (r.y1 - r.y0 + REVEAL_SENSOR_PADDING * 2.0).round() as i32,
                );
                let passthrough = anchored_passthrough_rect(
                    r,
                    self.cantus.config.layer_anchor,
                    self.cantus.logical_surface_size().1,
                );
                region.subtract(
                    passthrough.x0.round() as i32,
                    passthrough.y0.round() as i32,
                    (passthrough.x1 - passthrough.x0).round() as i32,
                    (passthrough.y1 - passthrough.y0).round() as i32,
                );
            }

            let translated_rects = input_bounds
                .filter(|_| sensor_active)
                .into_iter()
                .chain(self.cantus.input_rects().filter(|_| !sensor_active))
                .map(|r| r.translated_y(offset));
            for r in translated_rects {
                region.add(
                    r.x0.round() as i32,
                    r.y0.round() as i32,
                    (r.x1 - r.x0).round() as i32,
                    (r.y1 - r.y0).round() as i32,
                );
            }
            wl_surface.set_input_region(Some(&region));
            region.destroy();
            self.last_hitbox_hash = hash;
        }
    }
}

impl CantusApp {
    fn input_rects(&self) -> impl Iterator<Item = Rect> + '_ {
        self.playback.queue.iter().flat_map(|track| {
            track
                .runtime
                .rect(self.config.height)
                .into_iter()
                .chain(self.icon_row_rects(track).into_iter().flatten())
        })
    }

    fn input_bounds(&self) -> Option<Rect> {
        bounding_rects(self.input_rects())
    }

    fn pointer_over_contents(&self, surface_pos: glam::Vec2) -> bool {
        let offset = self.render.uniforms.content_offset.y;
        if self.config.auto_hide && offset.abs() >= 0.5 {
            self.input_bounds()
                .is_some_and(|rect| rect.translated_y(offset).contains(surface_pos))
        } else {
            self.input_rects()
                .any(|rect| rect.translated_y(offset).contains(surface_pos))
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for LayerShellApp {
    fn event(
        state: &mut Self,
        proxy: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, .. } => {
                proxy.ack_configure(serial);
                if width > 0 {
                    state.cantus.render.surface_width = Some(width as f32);
                }
                state.is_configured = true;
                state.update_scale_and_viewport();
                state.try_render_frame(qhandle);
            }
            zwlr_layer_surface_v1::Event::Closed => state.should_exit = true,
            _ => {}
        }
    }
}

impl Dispatch<WpFractionalScaleV1, ()> for LayerShellApp {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.cantus.render.scale = scale as f32 / 120.0;

            if state.is_configured {
                state.update_scale_and_viewport();
                state.try_render_frame(qhandle);
            }
        }
    }
}

impl Dispatch<WlCallback, ()> for LayerShellApp {
    fn event(
        state: &mut Self,
        _proxy: &WlCallback,
        event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_callback::Event::Done { .. }) && state.frame_callback.take().is_some()
        {
            state.try_render_frame(qhandle);
        }
    }
}

impl Dispatch<WlOutput, ()> for LayerShellApp {
    fn event(
        state: &mut Self,
        proxy: &WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let id = proxy.id();
        if let Some(info) = state.outputs.iter_mut().find(|info| info.handle.id() == id) {
            match event {
                wl_output::Event::Geometry { make, model, .. } => {
                    info.make_model = Some(format!("{make} {model}"));
                }
                wl_output::Event::Name { name } => info.name = Some(name),
                wl_output::Event::Description { description } => {
                    info.description = Some(description);
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for LayerShellApp {
    fn event(
        state: &mut Self,
        proxy: &WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event
            && let WEnum::Value(caps) = capabilities
        {
            if caps.contains(wl_seat::Capability::Pointer) {
                if state.pointer.is_none() {
                    state.pointer = Some(proxy.get_pointer(qhandle, ()));
                }
            } else if let Some(pointer) = state.pointer.take() {
                pointer.release();
            }
            if caps.contains(wl_seat::Capability::Keyboard) {
                if state.keyboard.is_none() {
                    state.keyboard = Some(proxy.get_keyboard(qhandle, ()));
                }
            } else if let Some(keyboard) = state.keyboard.take() {
                keyboard.release();
                state.keyboard_state = None;
                state.auto_hide.modifier_held = false;
                state.update_auto_hide_request();
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for LayerShellApp {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap {
                format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                fd,
                size,
            } => {
                let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                state.keyboard_state = unsafe {
                    xkb::Keymap::new_from_fd(
                        &context,
                        fd,
                        size as usize,
                        xkb::KEYMAP_FORMAT_TEXT_V1,
                        xkb::KEYMAP_COMPILE_NO_FLAGS,
                    )
                }
                .ok()
                .flatten()
                .map(|keymap| xkb::State::new(&keymap));
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(keyboard_state) = &mut state.keyboard_state {
                    keyboard_state.update_mask(
                        mods_depressed,
                        mods_latched,
                        mods_locked,
                        0,
                        0,
                        group,
                    );
                    state.auto_hide.modifier_held = keyboard_state.mod_name_is_active(
                        &state.cantus.config.auto_hide_modifier,
                        xkb::STATE_MODS_EFFECTIVE,
                    );
                    state.update_auto_hide_request();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for LayerShellApp {
    fn event(
        state: &mut Self,
        _proxy: &WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let surface_id = state.wl_surface.as_ref().map(Proxy::id);
        match event {
            wl_pointer::Event::Enter {
                surface,
                surface_x,
                surface_y,
                ..
            } if surface_id == Some(surface.id()) => {
                state.update_pointer_position(vec2(surface_x as f32, surface_y as f32));
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                let over_contents =
                    state.update_pointer_position(vec2(surface_x as f32, surface_y as f32));
                if over_contents {
                    state.cantus.handle_mouse_drag();
                } else {
                    state.cantus.cancel_drag();
                }
            }
            wl_pointer::Event::Leave { .. } => {
                state.cantus.interaction.mouse_pressure = 0.0;
                state.cantus.cancel_drag();
                // Moving the input hole is expected to produce Leave. Keep the
                // hide request latched; entering the surrounding sensor is the
                // signal that the pointer really moved away.
                if !state.cantus.config.auto_hide || !state.cantus.render.auto_hide_requested {
                    state
                        .auto_hide
                        .update_pointer(false, Instant::now(), Duration::ZERO);
                    state.update_auto_hide_request();
                }
            }
            wl_pointer::Event::Button {
                button,
                state: button_state,
                ..
            } => match (button, button_state) {
                (0x110, WEnum::Value(wl_pointer::ButtonState::Pressed)) => {
                    state.suppress_auto_hide_for_action();
                    state.cantus.left_click();
                }
                (0x110, WEnum::Value(wl_pointer::ButtonState::Released)) => {
                    state.cantus.left_click_released();
                }
                (0x111, WEnum::Value(wl_pointer::ButtonState::Pressed))
                    if state.cantus.interaction.dragging =>
                {
                    state.suppress_auto_hide_for_action();
                    state.cantus.right_click();
                }
                _ => {}
            },
            wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                discrete,
                ..
            }
            | wl_pointer::Event::AxisValue120 {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value120: discrete,
                ..
            } => {
                state.suppress_auto_hide_for_action();
                state.cantus.handle_scroll(discrete.signum());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AutoHideState, anchored_passthrough_rect, bounding_rects};
    use crate::{config::LayerAnchor, model::Rect};
    use std::time::{Duration, Instant};

    #[test]
    fn auto_hide_waits_for_deadline() {
        let now = Instant::now();
        let mut state = AutoHideState::default();
        state.update_pointer(true, now, Duration::from_millis(400));

        assert!(!state.should_hide(now + Duration::from_millis(399)));
        assert!(state.should_hide(now + Duration::from_millis(400)));
    }

    #[test]
    fn pointer_motion_does_not_restart_deadline() {
        let now = Instant::now();
        let mut state = AutoHideState::default();
        state.update_pointer(true, now, Duration::from_millis(400));
        state.update_pointer(
            true,
            now + Duration::from_millis(300),
            Duration::from_millis(400),
        );

        assert!(state.should_hide(now + Duration::from_millis(400)));
    }

    #[test]
    fn modifier_suppresses_hide_while_held() {
        let now = Instant::now();
        let mut state = AutoHideState::default();
        state.update_pointer(true, now, Duration::ZERO);
        state.modifier_held = true;
        assert!(!state.should_hide(now));

        state.modifier_held = false;
        assert!(state.should_hide(now));
    }

    #[test]
    fn action_suppresses_hide_until_pointer_leaves() {
        let now = Instant::now();
        let mut state = AutoHideState::default();
        state.update_pointer(true, now, Duration::ZERO);
        state.suppress_for_action();
        assert!(!state.should_hide(now));

        state.update_pointer(false, now, Duration::ZERO);
        state.update_pointer(true, now, Duration::ZERO);
        assert!(state.should_hide(now));
    }

    #[test]
    fn hidden_input_bounds_include_gaps_between_items() {
        let bounds = bounding_rects(
            [
                Rect::new(10.0, 20.0, 30.0, 40.0),
                Rect::new(50.0, 15.0, 70.0, 45.0),
            ]
            .into_iter(),
        )
        .unwrap();

        assert!(bounds.contains(glam::vec2(40.0, 30.0)));
        assert_eq!((bounds.x0, bounds.y0), (10.0, 15.0));
        assert_eq!((bounds.x1, bounds.y1), (70.0, 45.0));
    }

    #[test]
    fn passthrough_extends_toward_layer_anchor() {
        let bounds = Rect::new(10.0, 20.0, 70.0, 45.0);

        let top = anchored_passthrough_rect(bounds, LayerAnchor::Top, 100.0);
        assert_eq!((top.x0, top.y0, top.x1, top.y1), (10.0, 0.0, 70.0, 45.0));

        let bottom = anchored_passthrough_rect(bounds, LayerAnchor::Bottom, 100.0);
        assert_eq!(
            (bottom.x0, bottom.y0, bottom.x1, bottom.y1),
            (10.0, 20.0, 70.0, 100.0)
        );
    }
}

impl Dispatch<WlRegistry, ()> for LayerShellApp {
    fn event(
        state: &mut Self,
        proxy: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_ref() {
                "wl_compositor" => {
                    state.compositor =
                        Some(proxy.bind::<WlCompositor, (), Self>(name, version, qhandle, ()));
                }
                "zwlr_layer_shell_v1" => {
                    state.layer_shell =
                        Some(proxy.bind::<ZwlrLayerShellV1, (), Self>(name, 4, qhandle, ()));
                }
                "wp_viewporter" => {
                    state.viewporter =
                        Some(proxy.bind::<WpViewporter, (), Self>(name, 1, qhandle, ()));
                }
                "wp_fractional_scale_manager_v1" => {
                    state.fractional_manager = Some(
                        proxy.bind::<WpFractionalScaleManagerV1, (), Self>(name, 1, qhandle, ()),
                    );
                }
                "wl_seat" => {
                    state.seat =
                        Some(proxy.bind::<WlSeat, (), Self>(name, version.min(7), qhandle, ()));
                }
                "wl_output" => {
                    state.outputs.push(OutputInfo {
                        handle: proxy.bind::<WlOutput, (), Self>(name, version.min(4), qhandle, ()),
                        name: None,
                        description: None,
                        make_model: None,
                    });
                }
                _ => {}
            }
        }
    }
}

delegate_noop!(LayerShellApp: ignore WlSurface);
delegate_noop!(LayerShellApp: ignore ZwlrLayerShellV1);
delegate_noop!(LayerShellApp: ignore WpFractionalScaleManagerV1);
delegate_noop!(LayerShellApp: ignore WpViewporter);
delegate_noop!(LayerShellApp: ignore WpViewport);
delegate_noop!(LayerShellApp: ignore WlCompositor);
delegate_noop!(LayerShellApp: ignore WlRegion);
