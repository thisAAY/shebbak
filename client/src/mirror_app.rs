use crate::ipc::UpMessage;
use softbuffer::{Context, Surface};
use srw_core::mapping::InputMapping;
use srw_core::model::{OpenedWindow, WindowInfo};
use srw_core::pixels::BgraFrame;
use srw_core::protocol::{ClientMessage, HostMessage, MouseAction, MouseButton, WindowId};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton as WinitMouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId as WinitWindowId};

/// winit modifiers → CGEventFlags bits (macOS mask constants).
pub(crate) fn cg_flags(mods: ModifiersState) -> u64 {
    let mut f = 0u64;
    if mods.shift_key() {
        f |= 0x0002_0000; // kCGEventFlagMaskShift
    }
    if mods.control_key() {
        f |= 0x0004_0000; // kCGEventFlagMaskControl
    }
    if mods.alt_key() {
        f |= 0x0008_0000; // kCGEventFlagMaskAlternate
    }
    if mods.super_key() {
        f |= 0x0010_0000; // kCGEventFlagMaskCommand
    }
    f
}

/// Events pushed to the helper's UI thread by the IPC reader thread.
pub enum HelperEvent {
    Host(HostMessage),
    Frame {
        track_id: String,
        frame: BgraFrame,
    },
    Icon(Vec<u8>),
    /// Coordinator closed the socket (session over / coordinator quit).
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// Cmd-Q: stop mirroring this app (coordinator sends UnsubscribeApp).
    UserQuit,
    /// Last mirror window closed; coordinator may respawn us on demand.
    Idle,
    Eof,
}

pub const EXIT_CODE_IDLE: i32 = 42;

pub fn exit_code(reason: ExitReason) -> i32 {
    match reason {
        ExitReason::UserQuit | ExitReason::Eof => 0,
        ExitReason::Idle => EXIT_CODE_IDLE,
    }
}

/// Per-track rate limit for upstream DecodeProgress (the PLI stall
/// threshold is 1200 ms, so 250 ms granularity is plenty).
pub struct ProgressThrottle {
    last: std::collections::HashMap<String, std::time::Instant>,
}

impl ProgressThrottle {
    pub fn new() -> Self {
        Self {
            last: std::collections::HashMap::new(),
        }
    }
    pub fn allow(&mut self, track_id: &str, now: std::time::Instant) -> bool {
        match self.last.get(track_id) {
            Some(t) if now.duration_since(*t) < std::time::Duration::from_millis(250) => false,
            _ => {
                self.last.insert(track_id.to_string(), now);
                true
            }
        }
    }
}

impl Default for ProgressThrottle {
    fn default() -> Self {
        Self::new()
    }
}

/// What a mirror currently has to paint.
pub enum MirrorContent {
    None,
    Video(BgraFrame),
}

pub struct Mirror {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    content: MirrorContent,
    remote_id: WindowId,
    /// Title without the " (minimized)" suffix; re-applied on restore/rename.
    base_title: String,
    minimized: bool,
    /// Last size the HOST told us about — the echo guard for `Resized`, so a
    /// host-driven resize (which we apply via `request_inner_size`) doesn't
    /// bounce straight back out as a client-initiated `ResizeRequest`.
    last_host_size: (f64, f64),
    cursor: PhysicalPosition<f64>,
    /// Last time a `MouseMove` was sent for this mirror — coalesces
    /// `CursorMoved` down to ~60 Hz instead of forwarding every event.
    last_move_sent: Instant,
    /// Pointer-coordinate transform for the current stream state — identity
    /// except while an oversized child window letterboxes the frame.
    mapping: InputMapping,
}

pub struct HelperApp {
    ui_rx: Receiver<HelperEvent>,
    up_tx: std::sync::mpsc::Sender<UpMessage>,
    /// Live modifier state, updated on `WindowEvent::ModifiersChanged`;
    /// applied to every `KeyEvent` forwarded to the host.
    modifiers: ModifiersState,
    /// Frames that arrived before the mirror window existed, keyed by track_id.
    early_frames: HashMap<String, BgraFrame>,
    /// Last `InputMapping` seen for a remote window, keyed by remote id —
    /// kept even for windows with no live `Mirror` yet, since an
    /// `InputMapping` can arrive before the announcement/track pairing
    /// finishes and would otherwise be dropped with no re-send. Consulted
    /// by `create_mirror` to seed the mirror's mapping instead of assuming
    /// identity.
    pending_mappings: HashMap<WindowId, InputMapping>,
    mirrors: HashMap<WinitWindowId, Mirror>,
    by_remote: HashMap<WindowId, WinitWindowId>,
    by_track: HashMap<String, WinitWindowId>,
    /// Set when the event loop exits, so the binary can pick the right exit
    /// code (`exit_code`).
    exit_reason: Option<ExitReason>,
    /// Whether a mirror has ever been created — the idle-exit only fires
    /// once we've actually had a window (an empty helper freshly spawned
    /// with no windows yet is not "idle", it's still starting up).
    saw_a_window: bool,
    /// Whether any Dock icon has been applied yet — the Shebbak default in
    /// `resumed`, or the host's badged icon. Keeps a re-fired `resumed`
    /// (winit doesn't guarantee exactly one) from stomping the host icon.
    icon_applied: bool,
}

impl HelperApp {
    pub fn new(ui_rx: Receiver<HelperEvent>, up_tx: std::sync::mpsc::Sender<UpMessage>) -> Self {
        Self {
            ui_rx,
            up_tx,
            modifiers: ModifiersState::empty(),
            early_frames: HashMap::new(),
            pending_mappings: HashMap::new(),
            mirrors: HashMap::new(),
            by_remote: HashMap::new(),
            by_track: HashMap::new(),
            exit_reason: None,
            saw_a_window: false,
            icon_applied: false,
        }
    }

    /// Why the event loop exited, if it has.
    pub fn exit_reason(&self) -> Option<ExitReason> {
        self.exit_reason
    }

    /// Ordered relay to the coordinator — see `up_tx`.
    fn relay(&self, msg: ClientMessage) {
        let _ = self.up_tx.send(UpMessage::Relay { msg });
    }

    fn drain(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok(ev) = self.ui_rx.try_recv() {
            match ev {
                HelperEvent::Host(HostMessage::WindowOpened {
                    window_id,
                    title,
                    width,
                    height,
                    track_id,
                    app_id: _,
                }) => {
                    let ann = OpenedWindow {
                        info: WindowInfo {
                            id: window_id,
                            title,
                            x: 0.0,
                            y: 0.0,
                            width,
                            height,
                        },
                        track_id,
                    };
                    self.saw_a_window = true;
                    self.create_mirror(event_loop, ann);
                }
                HelperEvent::Frame { track_id, frame } => match self.by_track.get(&track_id) {
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
                HelperEvent::Host(HostMessage::WindowMinimized { window_id }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        m.minimized = true;
                        let title = format!("{} (minimized)", m.base_title);
                        m.window.set_title(&title);
                        // Keep the last frame frozen — content untouched.
                    }
                }
                HelperEvent::Host(HostMessage::WindowRestored { window_id }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        if m.minimized {
                            m.minimized = false;
                            let title = m.base_title.clone();
                            m.window.set_title(&title);
                        }
                    } // unknown id (e.g. the host's channel-open probe with id 0): ignore silently
                }
                HelperEvent::Host(HostMessage::WindowResized {
                    window_id,
                    width,
                    height,
                }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        m.last_host_size = (width, height);
                        let _ = m.window.request_inner_size(LogicalSize::new(width, height));
                    }
                }
                HelperEvent::Host(HostMessage::WindowTitleChanged { window_id, title }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        m.base_title = title.clone();
                        let shown = if m.minimized {
                            format!("{title} (minimized)")
                        } else {
                            title
                        };
                        m.window.set_title(&shown);
                    }
                }
                HelperEvent::Host(HostMessage::WindowClosed { window_id }) => {
                    info!("host closed window {window_id}");
                    self.destroy_mirror_by_remote(window_id);
                    if self.saw_a_window && self.mirrors.is_empty() {
                        // Last mirror gone: free this Dock tile. The
                        // coordinator keeps our app state and respawns a
                        // helper if the host app opens another window.
                        self.exit_reason = Some(ExitReason::Idle);
                        event_loop.exit();
                    }
                }
                HelperEvent::Host(HostMessage::InputMapping {
                    window_id,
                    scale_x,
                    scale_y,
                    offset_x,
                    offset_y,
                }) => {
                    let mapping = InputMapping {
                        scale_x,
                        scale_y,
                        offset_x,
                        offset_y,
                    };
                    // Always buffer, even if a live mirror also gets updated
                    // below: the mapping can arrive before the mirror exists
                    // (the announcement/track pairing races the host's
                    // send), and a dropped-here mapping would otherwise
                    // never be re-sent, stranding the client at identity.
                    self.pending_mappings.insert(window_id, mapping);
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        m.mapping = mapping;
                    }
                }
                HelperEvent::Host(HostMessage::AppAnnounced { .. })
                | HelperEvent::Host(HostMessage::SdpOffer { .. }) => {
                    // Never forwarded to helpers.
                }
                HelperEvent::Icon(png) => {
                    self.icon_applied = true;
                    // Badge the host icon with the Shebbak mark so this tile
                    // is distinguishable from a locally running copy of the
                    // same app; fall back to the raw icon if compositing
                    // fails.
                    match crate::dock::badge_icon(&png, crate::dock::SHEBBAK_ICON_SVG) {
                        Some(badged) => crate::dock::set_dock_icon(&badged),
                        None => crate::dock::set_dock_icon(&png),
                    }
                }
                HelperEvent::Eof => {
                    self.exit_reason = Some(ExitReason::Eof);
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
        if self.by_remote.contains_key(&ann.info.id) {
            // A second WindowOpened for a live remote id would overwrite
            // `by_remote` and strand the first Mirror unreachable (an
            // undecorated always-on-top window nothing can ever destroy).
            warn!(
                "duplicate WindowOpened for live remote window {}; ignoring",
                ann.info.id
            );
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(ann.info.title.clone())
            .with_inner_size(LogicalSize::new(ann.info.width, ann.info.height))
            .with_resizable(true) // bidirectional size sync
            // Host-initiated windows must never steal local focus from
            // unrelated local apps (spec §4); the user focuses the
            // mirror by clicking it.
            .with_active(false);
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
        // A mirror may seed from a frame that arrived just before it was created.
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
            base_title: ann.info.title.clone(),
            minimized: false,
            last_host_size: (ann.info.width, ann.info.height),
            cursor: PhysicalPosition::new(0.0, 0.0),
            last_move_sent: Instant::now(),
            mapping: self
                .pending_mappings
                .get(&ann.info.id)
                .copied()
                .unwrap_or(InputMapping::IDENTITY),
        };
        mirror.window.request_redraw();
        self.mirrors.insert(wid, mirror);
        self.by_remote.insert(ann.info.id, wid);
        self.by_track.insert(ann.track_id, wid);
        info!("mirror created for remote window {}", ann.info.id);
    }

    fn destroy_mirror_by_remote(&mut self, remote: WindowId) {
        self.pending_mappings.remove(&remote);
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
        let Some(m) = self.mirrors.get_mut(&wid) else {
            return;
        };
        let size = m.window.inner_size();
        let (Some(sw), Some(sh)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        if m.surface.resize(sw, sh).is_err() {
            return;
        }
        let Ok(mut buf) = m.surface.buffer_mut() else {
            return;
        };
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
        }
        let _ = buf.present();
    }
}

impl ApplicationHandler for HelperApp {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if !self.icon_applied {
            self.icon_applied = true;
            // Default to the Shebbak icon until the host's arrives — the
            // first-launch flash shows our mark instead of the generic
            // executable icon, and apps with no host icon keep it.
            crate::dock::set_dock_icon(crate::dock::SHEBBAK_ICON_SVG);
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, _ev: ()) {
        self.drain(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        wid: WinitWindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::RedrawRequested => self.redraw(wid),
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
            }
            WindowEvent::KeyboardInput { event, .. } => {
                use winit::keyboard::{KeyCode, PhysicalKey};
                use winit::platform::scancode::PhysicalKeyExtScancode;
                let Some(m) = self.mirrors.get(&wid) else {
                    return;
                };
                let down = event.state == ElementState::Pressed;
                // Local Cmd+Q quits the client; never forwarded.
                if down
                    && self.modifiers.super_key()
                    && event.physical_key == PhysicalKey::Code(KeyCode::KeyQ)
                {
                    self.exit_reason = Some(ExitReason::UserQuit);
                    event_loop.exit();
                    return;
                }
                // On macOS to_scancode() yields the Carbon virtual keycode == CGKeyCode.
                let Some(code) = event.physical_key.to_scancode() else {
                    return;
                };
                self.relay(ClientMessage::KeyEvent {
                    window_id: m.remote_id,
                    key_code: code as u16,
                    down,
                    flags: cg_flags(self.modifiers),
                });
                // Repeats forward as extra downs — posted CGEvents don't auto-repeat on the host.
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(m) = self.mirrors.get_mut(&wid) {
                    m.cursor = position;
                    if m.last_move_sent.elapsed() >= Duration::from_millis(16) {
                        m.last_move_sent = Instant::now();
                        let scale = m.window.scale_factor();
                        let (x, y) = m.mapping.apply(position.x / scale, position.y / scale);
                        let window_id = m.remote_id;
                        self.relay(ClientMessage::MouseMove { window_id, x, y });
                    }
                }
            }
            WindowEvent::Focused(true) => {
                if let Some(m) = self.mirrors.get(&wid) {
                    self.relay(ClientMessage::FocusChange {
                        window_id: m.remote_id,
                    });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(m) = self.mirrors.get(&wid) else {
                    return;
                };
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
                let (x, y) = m.mapping.apply(m.cursor.x / scale, m.cursor.y / scale);
                let msg = ClientMessage::MouseInput {
                    window_id: m.remote_id,
                    x,
                    y,
                    button,
                    action,
                };
                self.relay(msg);
            }
            WindowEvent::Resized(size) => {
                if let Some(m) = self.mirrors.get_mut(&wid) {
                    // Repaint at the current backing size. macOS emits Resized
                    // (and ScaleFactorChanged) right after a window is created
                    // and again once its scale settles; a mirror is created
                    // with_active(false) and never focused, so if its first
                    // paint landed at the wrong scale (video confined to a
                    // half-size corner, black around it) nothing else would
                    // repaint it — the user had to resize the window by hand.
                    // Redrawing on the automatic Resized/ScaleFactorChanged
                    // events macOS already delivers makes it self-correct.
                    m.window.request_redraw();
                    let scale = m.window.scale_factor();
                    debug!(
                        "mirror {} resized -> {}x{}px @scale {scale}",
                        m.remote_id, size.width, size.height
                    );
                    let (w, h) = (size.width as f64 / scale, size.height as f64 / scale);
                    // Echo guard: host-initiated resizes come back through
                    // `last_host_size`, so only a genuine client-side
                    // drag-resize should round-trip back to the host. Once
                    // we send, fold the just-sent size into the same guard
                    // — otherwise every intermediate frame of a live drag
                    // re-clears it and floods the host with a
                    // ResizeRequest per frame (and the host's own echo of
                    // this resize would otherwise pass the guard too).
                    if (w - m.last_host_size.0).abs() >= 1.0
                        || (h - m.last_host_size.1).abs() >= 1.0
                    {
                        m.last_host_size = (w, h);
                        let window_id = m.remote_id;
                        self.relay(ClientMessage::ResizeRequest {
                            window_id,
                            width: w,
                            height: h,
                        });
                    }
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The backing scale settled (or the window moved to a display
                // with a different scale). Repaint so the surface is rebuilt at
                // the correct device-pixel size instead of persisting a
                // wrong-scale first paint until a manual resize. See Resized.
                if let Some(m) = self.mirrors.get(&wid) {
                    debug!("mirror {} scale factor -> {scale_factor}", m.remote_id);
                    m.window.request_redraw();
                }
            }
            WindowEvent::CloseRequested => {
                if let Some(m) = self.mirrors.get(&wid) {
                    self.relay(ClientMessage::CloseRequest {
                        window_id: m.remote_id,
                    });
                    // Keep the mirror: it dies only on the host's WindowClosed.
                    // An unsaved-changes sheet will arrive as a new mirrored window.
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_bits_map_to_cg_masks() {
        use winit::keyboard::ModifiersState;
        assert_eq!(cg_flags(ModifiersState::empty()), 0);
        assert_eq!(cg_flags(ModifiersState::SHIFT), 0x0002_0000);
        assert_eq!(
            cg_flags(ModifiersState::SUPER | ModifiersState::SHIFT),
            0x0012_0000
        );
        assert_eq!(
            cg_flags(ModifiersState::CONTROL | ModifiersState::ALT),
            0x000C_0000
        );
    }

    #[test]
    fn exit_codes_distinguish_user_quit_from_idle() {
        assert_eq!(exit_code(ExitReason::UserQuit), 0);
        assert_eq!(exit_code(ExitReason::Idle), EXIT_CODE_IDLE);
        assert_eq!(exit_code(ExitReason::Eof), 0);
        assert_eq!(EXIT_CODE_IDLE, 42);
    }

    #[test]
    fn progress_throttle_limits_per_track_rate() {
        use std::time::{Duration, Instant};
        let mut t = ProgressThrottle::new();
        let t0 = Instant::now();
        assert!(t.allow("a", t0), "first report always passes");
        assert!(!t.allow("a", t0 + Duration::from_millis(100)));
        assert!(t.allow("b", t0), "tracks are independent");
        assert!(t.allow("a", t0 + Duration::from_millis(300)));
        assert!(!t.allow("a", t0 + Duration::from_millis(400)));
    }
}
