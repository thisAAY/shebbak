use crate::net::{send_client_msg, Net, UiEvent};
use softbuffer::{Context, Surface};
use srw_core::model::{OpenedWindow, TrackBinder, WindowInfo};
use srw_core::pixels::BgraFrame;
use srw_core::protocol::{ClientMessage, HostMessage, MouseAction, MouseButton, WindowId};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use tracing::{info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton as WinitMouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId as WinitWindowId};

pub struct Mirror {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    latest: Option<BgraFrame>,
    remote_id: WindowId,
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
                UiEvent::Host(HostMessage::WindowOpened { window_id, title, x: _, y: _, width, height, track_id }) => {
                    let opened = OpenedWindow {
                        info: WindowInfo { id: window_id, title, x: 0.0, y: 0.0, width, height },
                        track_id: track_id.clone(),
                    };
                    if let Some((ann, ())) = self.binder.on_announcement(opened) {
                        self.create_mirror(event_loop, ann);
                    }
                }
                UiEvent::TrackOpened { track_id } => {
                    if let Some((ann, ())) = self.binder.on_track(track_id, ()) {
                        self.create_mirror(event_loop, ann);
                    }
                }
                UiEvent::TrackFrame { track_id, frame } => {
                    match self.by_track.get(&track_id) {
                        Some(wid) => {
                            if let Some(m) = self.mirrors.get_mut(wid) {
                                m.latest = Some(frame);
                                m.window.request_redraw();
                            }
                        }
                        None => {
                            self.early_frames.insert(track_id, frame);
                        }
                    }
                }
                UiEvent::Host(HostMessage::WindowMoved { window_id, x, y }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        m.window.set_outer_position(LogicalPosition::new(x, y));
                    }
                }
                UiEvent::Host(HostMessage::WindowResized { window_id, width, height }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        let _ = m.window.request_inner_size(LogicalSize::new(width, height));
                    }
                }
                UiEvent::Host(HostMessage::WindowTitleChanged { window_id, title }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        m.window.set_title(&title);
                    }
                }
                UiEvent::Host(HostMessage::WindowClosed { window_id }) => {
                    info!("host closed window {window_id}");
                    self.destroy_mirror_by_remote(window_id);
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
        let attrs = Window::default_attributes()
            .with_title(ann.info.title.clone())
            .with_inner_size(LogicalSize::new(ann.info.width, ann.info.height));
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
        let mirror = Mirror {
            window,
            surface,
            latest: self.early_frames.remove(&ann.track_id),
            remote_id: ann.info.id,
            cursor: PhysicalPosition::new(0.0, 0.0),
        };
        mirror.window.request_redraw();
        self.mirrors.insert(wid, mirror);
        self.by_remote.insert(ann.info.id, wid);
        self.by_track.insert(ann.track_id, wid);
        info!("mirror created for remote window {}", ann.info.id);
    }

    fn destroy_mirror_by_remote(&mut self, remote: WindowId) {
        if let Some(wid) = self.by_remote.remove(&remote) {
            self.mirrors.remove(&wid);
            self.by_track.retain(|_, v| *v != wid);
        }
        if self.mirrors.is_empty() {
            eprintln!("all mirrors closed");
        }
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
        match &m.latest {
            None => buf.fill(0xFF202020),
            Some(frame) => {
                // Nearest-neighbor scale frame (BGRA) → buffer (0RGB u32).
                let (fw, fh) = (frame.width as usize, frame.height as usize);
                let (dw, dh) = (size.width as usize, size.height as usize);
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
        }
        let _ = buf.present();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _ev: ()) {
        self.drain(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, wid: WinitWindowId, event: WindowEvent) {
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
            WindowEvent::CloseRequested => {
                if let Some(m) = self.mirrors.get(&wid) {
                    send_client_msg(&self.net, ClientMessage::CloseWindow { window_id: m.remote_id });
                    let remote = m.remote_id;
                    self.destroy_mirror_by_remote(remote);
                }
                let _ = event_loop; // keep running; host will confirm via tracker if needed
            }
            _ => {}
        }
    }
}
