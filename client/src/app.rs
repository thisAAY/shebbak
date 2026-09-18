use crate::net::{send_client_msg, Net, UiEvent};
use softbuffer::{Context, Surface};
use srw_core::model::{OpenedWindow, TrackBinder, WindowInfo};
use srw_core::pixels::{BgraFrame, RgbaImage};
use srw_core::protocol::{
    ClientMessage, HostMessage, MouseAction, MouseButton, WindowId, WindowKind,
};
use std::collections::HashMap;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton as WinitMouseButton, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId as WinitWindowId, WindowLevel};

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

/// What a mirror currently has to paint. `Image` holds a decoded transient
/// PNG blit (Task 18).
pub enum MirrorContent {
    None,
    Video(BgraFrame),
    Image(RgbaImage),
}

/// Decode a transient blit PNG into an RGBA image. `EXPAND | ALPHA` forces
/// the output to RGBA8 regardless of the source's bit depth/color type, so
/// the `ColorType::Rgba` check below is a guarantee, not a guess.
fn decode_png(bytes: &[u8]) -> anyhow::Result<RgbaImage> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    anyhow::ensure!(
        info.color_type == png::ColorType::Rgba,
        "expected RGBA after transform"
    );
    buf.truncate(info.buffer_size());
    Ok(RgbaImage {
        width: info.width,
        height: info.height,
        data: buf,
    })
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
    /// Last time a `MouseMove` was sent for this mirror — coalesces
    /// `CursorMoved` down to ~60 Hz instead of forwarding every event.
    last_move_sent: Instant,
}

pub struct App {
    net: Net,
    ui_rx: Receiver<UiEvent>,
    binder: TrackBinder<()>,
    /// Live modifier state, updated on `WindowEvent::ModifiersChanged`;
    /// applied to every `KeyEvent` forwarded to the host.
    modifiers: ModifiersState,
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
            modifiers: ModifiersState::empty(),
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
                    window_id,
                    title,
                    kind,
                    parent_id,
                    offset_x,
                    offset_y,
                    width,
                    height,
                    track_id,
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
                UiEvent::Host(HostMessage::WindowResized {
                    window_id,
                    width,
                    height,
                }) => {
                    if let Some(m) = self.mirror_for_remote(window_id) {
                        m.last_host_size = (width, height);
                        let _ = m.window.request_inner_size(LogicalSize::new(width, height));
                    }
                }
                UiEvent::Host(HostMessage::WindowTitleChanged { window_id, title }) => {
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
                UiEvent::Host(HostMessage::WindowClosed { window_id }) => {
                    info!("host closed window {window_id}");
                    self.destroy_mirror_by_remote(window_id);
                }
                UiEvent::Host(HostMessage::TransientBlit { .. } | HostMessage::SdpOffer { .. }) => {
                    // TransientBlit: net.rs's BlitAssembler intercepts these
                    // chunks and surfaces assembled images as `UiEvent::Blit`
                    // below — this arm never actually receives one.
                    // SdpOffer: consumed inside ClientPeer itself and never
                    // actually forwarded here — listed for exhaustiveness.
                }
                UiEvent::Blit { window_id, png } => match decode_png(&png) {
                    Ok(img) => match self.by_remote.get(&window_id) {
                        Some(wid) => {
                            if let Some(m) = self.mirrors.get_mut(wid) {
                                m.content = MirrorContent::Image(img);
                                m.window.request_redraw();
                            }
                        }
                        None => {
                            // The host sends WindowOpened before starting the
                            // blit thread, and the data channel is ordered, so
                            // a blit for a window we don't know about is not a
                            // race — it's a straggler that arrived after
                            // WindowClosed already evicted the mirror. Drop it
                            // rather than buffering (buffering these leaked a
                            // full decoded image per closed transient forever).
                            debug!("dropping blit for unknown/closed window {window_id}");
                        }
                    },
                    Err(e) => warn!("blit decode failed for {window_id}: {e}"),
                },
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
                if let Some(parent) = ann
                    .parent_id
                    .and_then(|pid| self.by_remote.get(&pid))
                    .and_then(|wid| self.mirrors.get(wid))
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
        // Track-backed windows may seed from a frame that arrived just before
        // this mirror was created. Transients have no track and no analogous
        // early-blit buffer — see the Blit arm in `drain` for why.
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
            last_move_sent: Instant::now(),
        };
        mirror.window.request_redraw();
        self.mirrors.insert(wid, mirror);
        self.by_remote.insert(ann.info.id, wid);
        // Transient windows have no track (track_id == ""); don't let a
        // string collide multiple transients into the same `by_track` slot.
        if !ann.track_id.is_empty() {
            self.by_track.insert(ann.track_id, wid);
        }
        info!(
            "mirror created for remote window {} (kind {:?})",
            ann.info.id, ann.kind
        );
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
            MirrorContent::Image(img) => {
                // Nearest-neighbor scale image (RGBA, unpremultiplied) →
                // buffer (ARGB u32, premultiplied) for the compositor. The
                // window was created `with_transparent(true)` so a genuine
                // alpha byte lets softbuffer/macOS render real transparency
                // (rounded corners/shadows on menus come for free).
                let (fw, fh) = (img.width as usize, img.height as usize);
                if fw == 0 || fh == 0 {
                    return;
                }
                let (dw, dh) = (size.width as usize, size.height as usize);
                for dy in 0..dh {
                    let sy = dy * fh / dh;
                    for dx in 0..dw {
                        let sx = dx * fw / dw;
                        let si = (sy * fw + sx) * 4;
                        let (r, g, b, a) = (
                            img.data[si] as u32,
                            img.data[si + 1] as u32,
                            img.data[si + 2] as u32,
                            img.data[si + 3] as u32,
                        );
                        // Premultiply for the compositor; alpha in the top byte.
                        buf[dy * dw + dx] = (a << 24)
                            | ((r * a / 255) << 16)
                            | ((g * a / 255) << 8)
                            | (b * a / 255);
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
                    event_loop.exit();
                    return;
                }
                // On macOS to_scancode() yields the Carbon virtual keycode == CGKeyCode.
                let Some(code) = event.physical_key.to_scancode() else {
                    return;
                };
                send_client_msg(
                    &self.net,
                    ClientMessage::KeyEvent {
                        window_id: m.remote_id,
                        key_code: code as u16,
                        down,
                        flags: cg_flags(self.modifiers),
                    },
                );
                // Repeats forward as extra downs — posted CGEvents don't auto-repeat on the host.
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(m) = self.mirrors.get_mut(&wid) {
                    m.cursor = position;
                    if m.last_move_sent.elapsed() >= Duration::from_millis(16) {
                        m.last_move_sent = Instant::now();
                        let scale = m.window.scale_factor();
                        send_client_msg(
                            &self.net,
                            ClientMessage::MouseMove {
                                window_id: m.remote_id,
                                x: position.x / scale,
                                y: position.y / scale,
                            },
                        );
                    }
                }
            }
            WindowEvent::Focused(true) => {
                if let Some(m) = self.mirrors.get(&wid) {
                    if m.kind == WindowKind::Normal || m.kind == WindowKind::Sheet {
                        send_client_msg(
                            &self.net,
                            ClientMessage::FocusChange {
                                window_id: m.remote_id,
                            },
                        );
                    }
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
                if let Some(m) = self.mirrors.get_mut(&wid) {
                    if m.kind == WindowKind::Normal {
                        let scale = m.window.scale_factor();
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
                            send_client_msg(
                                &self.net,
                                ClientMessage::ResizeRequest {
                                    window_id: m.remote_id,
                                    width: w,
                                    height: h,
                                },
                            );
                        }
                    }
                }
            }
            WindowEvent::CloseRequested => {
                if let Some(m) = self.mirrors.get(&wid) {
                    send_client_msg(
                        &self.net,
                        ClientMessage::CloseRequest {
                            window_id: m.remote_id,
                        },
                    );
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
    use srw_core::blit::{chunk_blit, BlitAssembler};

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

    /// Encode a small RGBA image (with partial alpha, to exercise the
    /// premultiply path in `redraw`) as a real PNG, matching what the host's
    /// capture pipeline would produce for a transient window.
    fn encode_test_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(pixels).unwrap();
        }
        out
    }

    /// End-to-end check of the transient-blit path this task wires up:
    /// `chunk_blit` (host side) → `BlitAssembler::push` (net.rs's
    /// interception, exercised here the same way the `on_host_message`
    /// closure drives it) → `decode_png` (app.rs). A multi-chunk image
    /// forces the assembler to actually reassemble rather than pass through
    /// a single chunk.
    #[test]
    fn blit_roundtrip_through_assembler_and_decode() {
        // Noisy (not gradient) pixels so PNG's deflate compression can't
        // shrink this below one chunk — the point is to force a multi-chunk
        // reassembly, not a single pass-through.
        let (w, h) = (200u32, 200u32);
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        let mut state: u32 = 0x1234_5678;
        for _ in 0..(w * h) {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223); // LCG
            let bytes = state.to_le_bytes();
            pixels.extend_from_slice(&bytes);
        }
        let png_bytes = encode_test_png(w, h, &pixels);
        assert!(
            png_bytes.len() > srw_core::blit::BLIT_CHUNK_BYTES,
            "test image should span multiple chunks"
        );

        let chunks = chunk_blit(42, 1, &png_bytes);
        assert!(chunks.len() > 1);

        let mut assembler = BlitAssembler::new();
        let mut assembled = None;
        for chunk in &chunks {
            if let Some((window_id, png)) = assembler.push(chunk) {
                assert_eq!(window_id, 42);
                assembled = Some(png);
            }
        }
        let assembled = assembled.expect("assembler should complete once all chunks arrive");
        assert_eq!(assembled, png_bytes);

        let img = decode_png(&assembled).expect("decode_png should accept a real RGBA PNG");
        assert_eq!(img.width, w);
        assert_eq!(img.height, h);
        assert_eq!(img.data, pixels);
    }

    #[test]
    fn decode_png_rejects_garbage() {
        assert!(decode_png(b"not a png").is_err());
    }
}
