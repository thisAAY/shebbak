use crate::net::{send_client_msg, Net, UiEvent};
use softbuffer::{Context, Surface};
use srw_core::model::{OpenedWindow, TrackBinder, WindowInfo};
use srw_core::pixels::{BgraFrame, RgbaImage};
use srw_core::protocol::{ClientMessage, HostMessage, MouseAction, MouseButton, WindowId, WindowKind};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use tracing::{info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton as WinitMouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId as WinitWindowId, WindowLevel};

/// What a mirror currently has to paint. `Image` is populated by Task 18's
/// PNG-blit decode path (transient windows); until then it's never
/// constructed, only matched (kept exhaustive so `redraw` compiles today).
pub enum MirrorContent {
    None,
    Video(BgraFrame),
    #[allow(dead_code)]
    Image(RgbaImage),
}

pub struct Mirror {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    content: MirrorContent,
    remote_id: WindowId,
    kind: WindowKind,
    /// Title without the " (minimized)" suffix; re-applied on restore/rename.
    base_title: String,
    minimized: bool,
    /// Last size the HOST told us about — the echo guard for `Resized`, so a
    /// host-driven resize (which we apply via `request_inner_size`) doesn't
    /// bounce straight back out as a client-initiated `ResizeRequest`.
    last_host_size: (f64, f64),
    cursor: PhysicalPosition<f64>,
}

pub struct App {
    net: Net,
    ui_rx: Receiver<UiEvent>,
    binder: TrackBinder<()>,
    /// Frames that arrived before the mirror window existed, keyed by track_id.
    early_frames: HashMap<String, BgraFrame>,
    mirrors: HashMap<WinitWindowId, Mirror>,
    by_remote: HashMap<WindowId, WinitWindowId>,
    by_track: HashMap<String, WinitWindowId>,
    /// Set when the event loop exits because the peer disconnected, so `main`
    /// can exit nonzero for that case while a normal all-windows-closed exit
    /// stays 0 (spec: client exits on disconnect).
    disconnected: bool,
}

impl App {
    pub fn new(net: Net, ui_rx: Receiver<UiEvent>) -> Self {
        Self {
            net,
            ui_rx,
            binder: TrackBinder::new(),
            early_frames: HashMap::new(),
            mirrors: HashMap::new(),
            by_remote: HashMap::new(),
            by_track: HashMap::new(),
            disconnected: false,
        }
    }

    /// Whether the event loop exited because the peer disconnected.
    pub fn disconnected(&self) -> bool {
        self.disconnected
    }

    fn drain(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok(ev) = self.ui_rx.try_recv() {
            match ev {
                UiEvent::Host(HostMessage::WindowOpened {
                    window_id, title, kind, parent_id, offset_x, offset_y, width, height, track_id,
                }) => {
                    let ann = OpenedWindow {
                        info: WindowInfo { id: window_id, title, x: 0.0, y: 0.0, width, height },
                        kind,
                        parent_id,
                        offset: (offset_x, offset_y),
                        track_id: track_id.clone().unwrap_or_default(),
                    };
                    match track_id {
                        // Track-backed (Normal/Sheet): wait for announcement+track pair.
                        Some(_) => {
                            if let Some((ann, ())) = self.binder.on_announcement(ann) {
                                self.create_mirror(event_loop, ann);
                            }
                        }
                        // Transient: no track to wait for.
                        None => self.create_mirror(event_loop, ann),
                    }
                }
                UiEvent::TrackOpened { track_id } => {
                    if let Some((ann, ())) = self.binder.on_track(track_id, ()) {
                        self.create_mirror(event_loop, ann);
                    }
                }
                UiEvent::TrackFrame { track_id, frame } => match self.by_track.get(&track_id) {
                    Some(wid) => {
                        if let Some(m) = self.mirrors.get_mut(wid) {
                            m.content = MirrorContent::Video(frame);
                            m.window.request_redraw();
                        }
                    }
                    None => {
                        self.early_frames.insert(track_id, frame);
                    }
                },
                UiEvent::Host(HostMessage::WindowMinimized { window_id }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        m.minimized = true;
                        let title = format!("{} (minimized)", m.base_title);
                        m.window.set_title(&title);
                        // Keep the last frame frozen — content untouched.
                    }
                }
                UiEvent::Host(HostMessage::WindowRestored { window_id }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        if m.minimized {
                            m.minimized = false;
                            let title = m.base_title.clone();
                            m.window.set_title(&title);
                        }
                    } // unknown id (e.g. the host's channel-open probe with id 0): ignore silently
                }
                UiEvent::Host(HostMessage::WindowResized { window_id, width, height }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        m.last_host_size = (width, height);
                        let _ = m.window.request_inner_size(LogicalSize::new(width, height));
                    }
                }
                UiEvent::Host(HostMessage::WindowTitleChanged { window_id, title }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        m.base_title = title.clone();
                        let shown = if m.minimized { format!("{title} (minimized)") } else { title };
                        m.window.set_title(&shown);
                    }
                }
                UiEvent::Host(HostMessage::WindowClosed { window_id }) => {
                    info!("host closed window {window_id}");
                    self.destroy_mirror_by_remote(window_id);
                }
                UiEvent::Host(HostMessage::TransientBlit { .. } | HostMessage::SdpOffer { .. }) => {
                    // TransientBlit: decoded/rendered by Task 18's net layer.
                    // SdpOffer: consumed inside ClientPeer itself and never
                    // actually forwarded here — listed for exhaustiveness.
                }
                UiEvent::Disconnected => {
                    eprintln!("connection lost; exiting");
                    self.mirrors.clear();
                    self.by_remote.clear();
                    self.by_track.clear();
                    self.disconnected = true;
                    event_loop.exit();
                }
            }
        }
    }

    fn mirror_for_remote(&mut self, remote: WindowId) -> Option<&mut Mirror> {
        let wid = self.by_remote.get(&remote)?;
        self.mirrors.get_mut(wid)
    }

    fn create_mirror(&mut self, event_loop: &ActiveEventLoop, ann: OpenedWindow) {
        let attrs = match ann.kind {
            WindowKind::Normal => Window::default_attributes()
                .with_title(ann.info.title.clone())
                .with_inner_size(LogicalSize::new(ann.info.width, ann.info.height))
                .with_resizable(true) // bidirectional size sync
                // Host-initiated windows must never steal local focus from
                // unrelated local apps (spec §4); the user focuses the
                // mirror by clicking it.
                .with_active(false),
            WindowKind::Sheet | WindowKind::Transient => {
                let mut attrs = Window::default_attributes()
                    .with_title(ann.info.title.clone())
                    .with_inner_size(LogicalSize::new(ann.info.width, ann.info.height))
                    .with_resizable(false)
                    .with_decorations(false)
                    .with_window_level(WindowLevel::AlwaysOnTop)
                    .with_active(false); // never steal local focus
                if ann.kind == WindowKind::Transient {
                    attrs = attrs.with_transparent(true); // PNG alpha (Task 18)
                }
                // Parent-relative placement: parent mirror origin + host-point
                // offset, scaled by the PARENT mirror's own scale factor.
                if let Some(parent) =
                    ann.parent_id.and_then(|pid| self.by_remote.get(&pid)).and_then(|wid| self.mirrors.get(wid))
                {
                    if let Ok(pos) = parent.window.inner_position() {
                        let scale = parent.window.scale_factor();
                        attrs = attrs.with_position(LogicalPosition::new(
                            pos.x as f64 / scale + ann.offset.0,
                            pos.y as f64 / scale + ann.offset.1,
                        ));
                    } // parent unknown/position unavailable → let the WM place it
                }
                attrs
            }
        };
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                warn!("create window failed: {e}");
                return;
            }
        };
        let context = Context::new(window.clone()).expect("softbuffer context");
        let surface = Surface::new(&context, window.clone()).expect("softbuffer surface");
        let wid = window.id();
        let content = self
            .early_frames
            .remove(&ann.track_id)
            .map(MirrorContent::Video)
            .unwrap_or(MirrorContent::None);
        let mirror = Mirror {
            window,
            surface,
            content,
            remote_id: ann.info.id,
            kind: ann.kind,
            base_title: ann.info.title.clone(),
            minimized: false,
            last_host_size: (ann.info.width, ann.info.height),
            cursor: PhysicalPosition::new(0.0, 0.0),
        };
        mirror.window.request_redraw();
        self.mirrors.insert(wid, mirror);
        self.by_remote.insert(ann.info.id, wid);
        // Transient windows have no track (track_id == ""); don't let a
        // string collide multiple transients into the same `by_track` slot.
        if !ann.track_id.is_empty() {
            self.by_track.insert(ann.track_id, wid);
        }
        info!("mirror created for remote window {} (kind {:?})", ann.info.id, ann.kind);
    }

    fn destroy_mirror_by_remote(&mut self, remote: WindowId) {
        if let Some(wid) = self.by_remote.remove(&remote) {
            self.mirrors.remove(&wid);
            // Collect the track ids bound to this mirror before dropping them
            // from `by_track` (retain alone would discard the keys), so any
            // late frames still landing in `early_frames` for this track can
            // be evicted too — otherwise a closed track's `read_track` task
            // keeps decoding until the host tears it down, and every frame
            // that misses the (now gone) `by_track` entry re-buffers itself
            // into `early_frames` forever (one leaked frame per closed
            // window for the rest of the process lifetime).
            let track_ids: Vec<String> = self
                .by_track
                .iter()
                .filter(|(_, v)| **v == wid)
                .map(|(k, _)| k.clone())
                .collect();
            for track_id in track_ids {
                self.by_track.remove(&track_id);
                self.early_frames.remove(&track_id);
            }
        }
        // With app sharing, zero mirrors is a valid idle state — the event
        // loop keeps running and a new host window revives it.
    }

    fn redraw(&mut self, wid: WinitWindowId) {
        let Some(m) = self.mirrors.get_mut(&wid) else { return };
        let size = m.window.inner_size();
        let (Some(sw), Some(sh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if m.surface.resize(sw, sh).is_err() {
            return;
        }
        let Ok(mut buf) = m.surface.buffer_mut() else { return };
        match &m.content {
            MirrorContent::None => buf.fill(0xFF202020),
            MirrorContent::Video(frame) => {
                // Nearest-neighbor scale frame (BGRA) → buffer (0RGB u32).
                let (fw, fh) = (frame.width as usize, frame.height as usize);
                let (dw, dh) = (size.width as usize, size.height as usize);
                if fw == 0 || fh == 0 || dw == 0 || dh == 0 {
                    return;
                }
                for dy in 0..dh {
                    let sy = dy * fh / dh;
                    for dx in 0..dw {
                        let sx = dx * fw / dw;
                        let si = (sy * fw + sx) * 4;
                        let b = frame.data[si] as u32;
                        let g = frame.data[si + 1] as u32;
                        let r = frame.data[si + 2] as u32;
                        buf[dy * dw + dx] = (r << 16) | (g << 8) | b;
                    }
                }
            }
            // Task 18 decodes the transient PNG blit into this; stay blank
            // until then.
            MirrorContent::Image(_) => buf.fill(0xFF202020),
        }
        let _ = buf.present();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _ev: ()) {
        self.drain(event_loop);
    }

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, wid: WinitWindowId, event: WindowEvent) {
        match event {
            WindowEvent::RedrawRequested => self.redraw(wid),
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(m) = self.mirrors.get_mut(&wid) {
                    m.cursor = position;
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(m) = self.mirrors.get(&wid) else { return };
                let button = match button {
                    WinitMouseButton::Left => MouseButton::Left,
                    WinitMouseButton::Right => MouseButton::Right,
                    _ => return,
                };
                let action = match state {
                    ElementState::Pressed => MouseAction::Down,
                    ElementState::Released => MouseAction::Up,
                };
                let scale = m.window.scale_factor();
                let msg = ClientMessage::MouseInput {
                    window_id: m.remote_id,
                    x: m.cursor.x / scale,
                    y: m.cursor.y / scale,
                    button,
                    action,
                };
                send_client_msg(&self.net, msg);
            }
            WindowEvent::Resized(size) => {
                if let Some(m) = self.mirrors.get(&wid) {
                    if m.kind == WindowKind::Normal {
                        let scale = m.window.scale_factor();
                        let (w, h) = (size.width as f64 / scale, size.height as f64 / scale);
                        // Echo guard: host-initiated resizes come back through
                        // `last_host_size`, so only a genuine client-side
                        // drag-resize should round-trip back to the host.
                        if (w - m.last_host_size.0).abs() >= 1.0 || (h - m.last_host_size.1).abs() >= 1.0 {
                            send_client_msg(
                                &self.net,
                                ClientMessage::ResizeRequest { window_id: m.remote_id, width: w, height: h },
                            );
                        }
                    }
                }
            }
            WindowEvent::CloseRequested => {
                if let Some(m) = self.mirrors.get(&wid) {
                    send_client_msg(&self.net, ClientMessage::CloseRequest { window_id: m.remote_id });
                    // Keep the mirror: it dies only on the host's WindowClosed.
                    // An unsaved-changes sheet will arrive as a new mirrored window.
                }
            }
            _ => {}
        }
    }
}
