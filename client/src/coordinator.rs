//! Coordinator-side helper management: one mirror-helper process per
//! shared host app, spawned from its stub bundle, fed over a socketpair.

use crate::bundle::{ensure_bundle, StubBundle};
use crate::ipc::{self, DownFrame, UpMessage, HELPER_FD};
use crate::mirror_app::EXIT_CODE_IDLE;
use crate::router::VideoRouter;
use anyhow::{Context as _, Result};
use base64::Engine as _;
use srw_core::protocol::{AppId, ClientMessage, HostMessage, WindowId};
use std::collections::{HashMap, HashSet};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Events drained by the coordinator's winit loop (Task 11).
pub enum CoordEvent {
    Net(crate::net::UiEvent),
    HelperUp { app_id: AppId, msg: UpMessage },
    HelperExited { app_id: AppId, code: Option<i32> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitAction {
    Unsubscribe,
    Reap,
    Respawn,
    GiveUp,
}

pub(crate) fn exit_action(code: Option<i32>, respawned_once: bool) -> ExitAction {
    match code {
        Some(0) => ExitAction::Unsubscribe,
        Some(c) if c == EXIT_CODE_IDLE => ExitAction::Reap,
        _ if !respawned_once => ExitAction::Respawn,
        _ => ExitAction::GiveUp,
    }
}

pub(crate) struct WindowReplay {
    pub title: String,
    pub width: f64,
    pub height: f64,
    pub track_id: String,
    pub minimized: bool,
    /// The last InputMapping HostMessage forwarded, if any.
    pub mapping: Option<HostMessage>,
}

struct RunningHelper {
    /// Feeds the helper's writer thread; dropping it shuts the socket's
    /// write half down, which the helper reads as EOF.
    down_tx: Sender<DownFrame>,
}

pub(crate) struct AppState {
    app_id: AppId,
    name: String,
    icon_png: Vec<u8>,
    bundle: StubBundle,
    running: Option<RunningHelper>,
    pub(crate) windows: HashMap<WindowId, WindowReplay>,
    respawned_once: bool,
    given_up: bool,
}

#[cfg(test)]
impl AppState {
    pub(crate) fn for_test(name: &str, icon_png: Vec<u8>) -> Self {
        Self {
            app_id: 1,
            name: name.into(),
            icon_png,
            bundle: StubBundle {
                dir: PathBuf::new(),
                exe: PathBuf::new(),
            },
            running: None,
            windows: HashMap::new(),
            respawned_once: false,
            given_up: false,
        }
    }
}

/// The DownFrames that rebuild a helper's world after (re)spawn, in a
/// deterministic order: icon, then each window (sorted by id) as
/// WindowOpened [+ WindowMinimized] [+ InputMapping].
pub(crate) fn replay_frames(state: &AppState) -> Vec<DownFrame> {
    let mut frames = Vec::new();
    if !state.icon_png.is_empty() {
        frames.push(DownFrame::Icon(state.icon_png.clone()));
    }
    let mut ids: Vec<WindowId> = state.windows.keys().copied().collect();
    ids.sort_unstable();
    for id in ids {
        let w = &state.windows[&id];
        frames.push(DownFrame::Control(HostMessage::WindowOpened {
            window_id: id,
            title: w.title.clone(),
            width: w.width,
            height: w.height,
            track_id: w.track_id.clone(),
            app_id: state.app_id,
        }));
        if w.minimized {
            frames.push(DownFrame::Control(HostMessage::WindowMinimized {
                window_id: id,
            }));
        }
        if let Some(mapping) = &w.mapping {
            frames.push(DownFrame::Control(mapping.clone()));
        }
    }
    frames
}

/// The Icon frame to send right after the very first successful spawn for
/// a newly announced app (before it has any windows). Respawns never call
/// this — they rely solely on `replay_frames`, which already includes the
/// icon — so a helper is sent its icon exactly once per (re)spawn.
pub(crate) fn initial_icon_frame(state: &AppState) -> Option<DownFrame> {
    (!state.icon_png.is_empty()).then(|| DownFrame::Icon(state.icon_png.clone()))
}

pub struct HelperManager {
    router: Arc<VideoRouter>,
    to_host: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
    events: Sender<CoordEvent>,
    wake: Arc<dyn Fn() + Send + Sync>,
    helper_bin: PathBuf,
    mirrors_dir: PathBuf,
    apps: HashMap<AppId, AppState>,
    app_of_window: HashMap<WindowId, AppId>,
    app_of_track: HashMap<String, AppId>,
    taken_dirs: HashSet<PathBuf>,
    /// InputMapping seen before its WindowOpened (announcement/track races).
    pending_mappings: HashMap<WindowId, HostMessage>,
}

impl HelperManager {
    pub fn new(
        router: Arc<VideoRouter>,
        to_host: tokio::sync::mpsc::UnboundedSender<ClientMessage>,
        events: Sender<CoordEvent>,
        wake: Arc<dyn Fn() + Send + Sync>,
        helper_bin: PathBuf,
        mirrors_dir: PathBuf,
    ) -> Self {
        Self {
            router,
            to_host,
            events,
            wake,
            helper_bin,
            mirrors_dir,
            apps: HashMap::new(),
            app_of_window: HashMap::new(),
            app_of_track: HashMap::new(),
            taken_dirs: HashSet::new(),
            pending_mappings: HashMap::new(),
        }
    }

    pub fn on_host_message(&mut self, msg: HostMessage) {
        match msg {
            HostMessage::AppAnnounced {
                app_id,
                name,
                icon_png,
            } => {
                if self.apps.contains_key(&app_id) {
                    warn!("duplicate AppAnnounced for app {app_id}; ignoring");
                    return;
                }
                let icon = base64::engine::general_purpose::STANDARD
                    .decode(icon_png.as_bytes())
                    .unwrap_or_default();
                let bundle = match ensure_bundle(
                    &self.mirrors_dir,
                    &name,
                    &self.helper_bin,
                    &self.taken_dirs,
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        error!("stub bundle for '{name}': {e:#}; app {app_id} will not mirror");
                        return;
                    }
                };
                self.taken_dirs.insert(bundle.dir.clone());
                let mut state = AppState {
                    app_id,
                    name,
                    icon_png: icon,
                    bundle,
                    running: None,
                    windows: HashMap::new(),
                    respawned_once: false,
                    given_up: false,
                };
                match self.spawn(&mut state) {
                    Ok(()) => {
                        // The only icon send for this app's initial spawn;
                        // every later (re)spawn's icon comes from
                        // `replay_frames` inside `spawn_and_replay`.
                        if let Some(frame) = initial_icon_frame(&state) {
                            if let Some(running) = state.running.as_ref() {
                                let _ = running.down_tx.send(frame);
                            }
                        }
                    }
                    Err(e) => error!("spawn helper for '{}': {e:#}", state.name),
                }
                self.apps.insert(app_id, state);
            }
            HostMessage::WindowOpened {
                window_id,
                title,
                width,
                height,
                track_id,
                app_id,
            } => {
                let Some(state) = self.apps.get_mut(&app_id) else {
                    warn!("WindowOpened for unknown app {app_id}; dropping");
                    return;
                };
                if state.given_up {
                    return;
                }
                let pending = self.pending_mappings.get(&window_id).cloned();
                state.windows.insert(
                    window_id,
                    WindowReplay {
                        title: title.clone(),
                        width,
                        height,
                        track_id: track_id.clone(),
                        minimized: false,
                        mapping: pending,
                    },
                );
                let was_dormant = state.running.is_none();
                self.app_of_window.insert(window_id, app_id);
                self.app_of_track.insert(track_id.clone(), app_id);
                if was_dormant {
                    // Dormant (idle-exited) helper: revive it. replay_frames
                    // includes the just-inserted window, so don't also
                    // forward the message below.
                    if let Err(e) = self.spawn_and_replay(app_id) {
                        error!("respawn for app {app_id}: {e:#}");
                    }
                    return;
                }
                self.forward(
                    app_id,
                    DownFrame::Control(HostMessage::WindowOpened {
                        window_id,
                        title,
                        width,
                        height,
                        track_id: track_id.clone(),
                        app_id,
                    }),
                );
                if let Some(mapping) = self.pending_mappings.remove(&window_id) {
                    self.forward(app_id, DownFrame::Control(mapping));
                }
                self.route_track(&track_id, app_id);
            }
            HostMessage::WindowResized {
                window_id,
                width,
                height,
            } => {
                if let Some(app_id) = self.app_of_window.get(&window_id).copied() {
                    if let Some(w) = self
                        .apps
                        .get_mut(&app_id)
                        .and_then(|s| s.windows.get_mut(&window_id))
                    {
                        w.width = width;
                        w.height = height;
                    }
                    self.forward(
                        app_id,
                        DownFrame::Control(HostMessage::WindowResized {
                            window_id,
                            width,
                            height,
                        }),
                    );
                }
            }
            HostMessage::WindowTitleChanged { window_id, title } => {
                if let Some(app_id) = self.app_of_window.get(&window_id).copied() {
                    if let Some(w) = self
                        .apps
                        .get_mut(&app_id)
                        .and_then(|s| s.windows.get_mut(&window_id))
                    {
                        w.title = title.clone();
                    }
                    self.forward(
                        app_id,
                        DownFrame::Control(HostMessage::WindowTitleChanged { window_id, title }),
                    );
                }
            }
            HostMessage::WindowMinimized { window_id } => {
                if let Some(app_id) = self.app_of_window.get(&window_id).copied() {
                    if let Some(w) = self
                        .apps
                        .get_mut(&app_id)
                        .and_then(|s| s.windows.get_mut(&window_id))
                    {
                        w.minimized = true;
                    }
                    self.forward(
                        app_id,
                        DownFrame::Control(HostMessage::WindowMinimized { window_id }),
                    );
                }
                // Unknown id (e.g. the host's channel-open probe, id 0):
                // ignore silently, as the single-process client did.
            }
            HostMessage::WindowRestored { window_id } => {
                if let Some(app_id) = self.app_of_window.get(&window_id).copied() {
                    if let Some(w) = self
                        .apps
                        .get_mut(&app_id)
                        .and_then(|s| s.windows.get_mut(&window_id))
                    {
                        w.minimized = false;
                    }
                    self.forward(
                        app_id,
                        DownFrame::Control(HostMessage::WindowRestored { window_id }),
                    );
                }
                // Unknown id (e.g. the host's channel-open probe, id 0):
                // ignore silently, as the single-process client did.
            }
            HostMessage::WindowClosed { window_id } => {
                self.pending_mappings.remove(&window_id);
                if let Some(app_id) = self.app_of_window.remove(&window_id) {
                    if let Some(state) = self.apps.get_mut(&app_id) {
                        if let Some(w) = state.windows.remove(&window_id) {
                            self.router.clear_route(&w.track_id);
                            self.app_of_track.remove(&w.track_id);
                        }
                    }
                    self.forward(
                        app_id,
                        DownFrame::Control(HostMessage::WindowClosed { window_id }),
                    );
                }
            }
            HostMessage::InputMapping {
                window_id,
                scale_x,
                scale_y,
                offset_x,
                offset_y,
            } => {
                let msg = HostMessage::InputMapping {
                    window_id,
                    scale_x,
                    scale_y,
                    offset_x,
                    offset_y,
                };
                match self.app_of_window.get(&window_id).copied() {
                    Some(app_id) => {
                        if let Some(w) = self
                            .apps
                            .get_mut(&app_id)
                            .and_then(|s| s.windows.get_mut(&window_id))
                        {
                            w.mapping = Some(msg.clone());
                        }
                        self.forward(app_id, DownFrame::Control(msg));
                    }
                    None => {
                        // Buffer: can arrive before WindowOpened (host race).
                        self.pending_mappings.insert(window_id, msg);
                    }
                }
            }
            HostMessage::SdpOffer { .. } => {
                // Consumed inside ClientPeer; never reaches the manager.
            }
        }
    }

    pub fn on_helper_up(&mut self, app_id: AppId, msg: UpMessage) {
        match msg {
            UpMessage::Relay { msg } => {
                let _ = self.to_host.send(msg);
            }
            UpMessage::DecodeProgress { track_id } => {
                // Only accept progress for tracks this app actually owns.
                if self.app_of_track.get(&track_id) == Some(&app_id) {
                    self.router.mark_progress(&track_id);
                }
            }
        }
    }

    pub fn on_helper_exited(&mut self, app_id: AppId, code: Option<i32>) {
        let Some(state) = self.apps.get_mut(&app_id) else {
            return;
        };
        state.running = None;
        match exit_action(code, state.respawned_once) {
            ExitAction::Unsubscribe => {
                info!(
                    "app {app_id} ('{}') quit by user; unsubscribing",
                    state.name
                );
                state.given_up = true;
                self.drop_app_windows(app_id);
                let _ = self.to_host.send(ClientMessage::UnsubscribeApp { app_id });
            }
            ExitAction::Reap => {
                info!("app {app_id} ('{}') idle; helper reaped", state.name);
                // Windows are already gone (that's why it idled); state
                // stays for an on-demand respawn.
            }
            ExitAction::Respawn => {
                warn!(
                    "helper for app {app_id} ('{}') crashed (code {code:?}); respawning once",
                    state.name
                );
                state.respawned_once = true;
                if let Err(e) = self.spawn_and_replay(app_id) {
                    error!("respawn failed: {e:#}");
                }
            }
            ExitAction::GiveUp => {
                error!(
                    "helper for app {app_id} ('{}') crashed twice; giving up on it",
                    state.name
                );
                state.given_up = true;
                self.drop_app_windows(app_id);
                let _ = self.to_host.send(ClientMessage::UnsubscribeApp { app_id });
            }
        }
    }

    /// Session teardown: dropping every down_tx shuts each socket's write
    /// half, which helpers read as EOF and exit on.
    pub fn shutdown_all(&mut self) {
        for state in self.apps.values_mut() {
            state.running = None;
        }
    }

    fn drop_app_windows(&mut self, app_id: AppId) {
        let Some(state) = self.apps.get_mut(&app_id) else {
            return;
        };
        for (id, w) in state.windows.drain() {
            self.router.clear_route(&w.track_id);
            self.app_of_track.remove(&w.track_id);
            self.app_of_window.remove(&id);
        }
    }

    fn route_track(&self, track_id: &str, app_id: AppId) {
        if let Some(tx) = self.apps.get(&app_id).and_then(|s| s.running.as_ref()) {
            self.router.set_route(track_id, tx.down_tx.clone());
        }
    }

    fn spawn_and_replay(&mut self, app_id: AppId) -> Result<()> {
        // Frames are computed before the spawn so the just-updated window
        // state is what gets replayed.
        let frames = replay_frames(&self.apps[&app_id]);
        self.spawn_inner(app_id)?;
        if let Some(running) = self.apps[&app_id].running.as_ref() {
            for f in frames {
                let _ = running.down_tx.send(f);
            }
        }
        // Re-route every track to the new helper and re-arm the PLI loop
        // so its fresh decoders get an IDR immediately.
        let tracks: Vec<String> = self.apps[&app_id]
            .windows
            .values()
            .map(|w| w.track_id.clone())
            .collect();
        for t in tracks {
            self.route_track(&t, app_id);
            self.router.clear_progress(&t);
        }
        Ok(())
    }

    fn spawn(&mut self, state: &mut AppState) -> Result<()> {
        let (coord_end, helper_end) = UnixStream::pair().context("socketpair")?;
        let fd = helper_end.as_raw_fd();
        let mut cmd = Command::new(&state.bundle.exe);
        // dup2 clears CLOEXEC on the new fd; the original helper_end fd
        // (CLOEXEC) closes at exec, and the parent's copy drops below.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(fd, HELPER_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("spawn helper {}", state.bundle.exe.display()))?;
        drop(helper_end);
        // Captured before handing `child` off to `wire_helper`: on any
        // failure there, `child` may already be gone (dropped inside a
        // thread closure that was never run — `Builder::spawn` drops an
        // unrun closure on failure without invoking it), so the pid is
        // the only reliable handle left to reap the process by.
        let pid = child.id();
        info!("helper for '{}' spawned (pid {pid})", state.name);

        match self.wire_helper(state, coord_end, child) {
            Ok(down_tx) => {
                state.running = Some(RunningHelper { down_tx });
                Ok(())
            }
            Err(e) => {
                // The process is already forked+exec'd but no reaper
                // thread ended up owning it — wait() it ourselves so it
                // doesn't linger as a zombie (or an orphan, if it's still
                // running) once it exits.
                reap_by_pid(pid);
                Err(e)
            }
        }
    }

    /// Wires a freshly spawned helper's socket to writer/reader threads and
    /// installs a reaper thread that owns `child`. Returns the channel that
    /// feeds the writer thread. Every step here is fallible; the caller
    /// (`spawn`) reaps `child` by pid if this returns `Err`, since `child`
    /// itself may already have been dropped inside a thread closure that
    /// never ran (the reaper-thread-spawn failure case).
    fn wire_helper(
        &self,
        state: &AppState,
        coord_end: UnixStream,
        child: std::process::Child,
    ) -> Result<Sender<DownFrame>> {
        // Writer thread: sole owner of the write direction. On channel
        // close (RunningHelper dropped) it shuts down the write half so
        // the helper sees EOF even while our reader clone stays open.
        let (down_tx, down_rx) = std::sync::mpsc::channel::<DownFrame>();
        let mut write_stream = coord_end.try_clone().context("clone socket")?;
        std::thread::Builder::new()
            .name(format!("helper-writer-{}", state.app_id))
            .spawn(move || {
                for f in down_rx {
                    if ipc::write_down(&mut write_stream, &f).is_err() {
                        break;
                    }
                }
                let _ = write_stream.shutdown(std::net::Shutdown::Write);
            })
            .context("spawn writer thread")?;

        // Reader thread: upstream messages → coordinator event queue.
        {
            let app_id = state.app_id;
            let events = self.events.clone();
            let wake = self.wake.clone();
            let mut read_stream = coord_end;
            std::thread::Builder::new()
                .name(format!("helper-reader-{app_id}"))
                .spawn(move || loop {
                    match ipc::read_up(&mut read_stream) {
                        Ok(Some(msg)) => {
                            let _ = events.send(CoordEvent::HelperUp { app_id, msg });
                            wake();
                        }
                        Ok(None) => return,
                        Err(e) => {
                            warn!("helper {app_id} upstream read: {e:#}");
                            return;
                        }
                    }
                })
                .context("spawn reader thread")?;
        }

        // Reaper thread: exit status → coordinator event queue. Takes
        // ownership of `child` as the last fallible step here.
        {
            let app_id = state.app_id;
            let events = self.events.clone();
            let wake = self.wake.clone();
            let mut child = child;
            std::thread::Builder::new()
                .name(format!("helper-reaper-{app_id}"))
                .spawn(move || {
                    let code = child.wait().ok().and_then(|s| s.code());
                    let _ = events.send(CoordEvent::HelperExited { app_id, code });
                    wake();
                })
                .context("spawn reaper thread")?;
        }

        Ok(down_tx)
    }

    fn spawn_inner(&mut self, app_id: AppId) -> Result<()> {
        let mut state = self.apps.remove(&app_id).expect("caller checked");
        let r = self.spawn(&mut state);
        self.apps.insert(app_id, state);
        r
    }

    fn forward(&self, app_id: AppId, frame: DownFrame) {
        if let Some(tx) = self.apps.get(&app_id).and_then(|s| s.running.as_ref()) {
            let _ = tx.down_tx.send(frame);
        }
    }
}

/// Best-effort reap of a helper process the coordinator failed to hand off
/// to a reaper thread. Used only on a `spawn()` failure path after the
/// process was already forked+exec'd — a failure here just means the
/// process was already gone.
fn reap_by_pid(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
        let mut status: libc::c_int = 0;
        libc::waitpid(pid as libc::pid_t, &mut status, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use srw_core::protocol::HostMessage;

    #[test]
    fn exit_action_policy() {
        // Cmd-Q: helper chose to stop mirroring this app.
        assert_eq!(exit_action(Some(0), false), ExitAction::Unsubscribe);
        assert_eq!(exit_action(Some(0), true), ExitAction::Unsubscribe);
        // Idle: all windows closed; keep state, respawn on demand.
        assert_eq!(exit_action(Some(EXIT_CODE_IDLE), false), ExitAction::Reap);
        assert_eq!(exit_action(Some(EXIT_CODE_IDLE), true), ExitAction::Reap);
        // Crash: respawn once, then give up.
        assert_eq!(exit_action(Some(101), false), ExitAction::Respawn);
        assert_eq!(exit_action(Some(101), true), ExitAction::GiveUp);
        // Killed by signal (no code): same as crash.
        assert_eq!(exit_action(None, false), ExitAction::Respawn);
        assert_eq!(exit_action(None, true), ExitAction::GiveUp);
    }

    #[test]
    fn replay_reconstructs_windows_minimize_and_mapping() {
        let mut state = AppState::for_test("Safari", vec![0x89, b'P']);
        state.windows.insert(
            10,
            WindowReplay {
                title: "Doc A".into(),
                width: 800.0,
                height: 600.0,
                track_id: "win-10".into(),
                minimized: false,
                mapping: None,
            },
        );
        state.windows.insert(
            11,
            WindowReplay {
                title: "Doc B".into(),
                width: 400.0,
                height: 300.0,
                track_id: "win-11".into(),
                minimized: true,
                mapping: Some(HostMessage::InputMapping {
                    window_id: 11,
                    scale_x: 1.5,
                    scale_y: 1.5,
                    offset_x: -10.0,
                    offset_y: 0.0,
                }),
            },
        );
        let frames = replay_frames(&state);
        // Icon first, then per-window (ordered by window id for
        // determinism): WindowOpened, then WindowMinimized / InputMapping
        // as applicable.
        assert!(matches!(&frames[0], DownFrame::Icon(png) if png == &vec![0x89, b'P']));
        match &frames[1] {
            DownFrame::Control(HostMessage::WindowOpened {
                window_id: 10,
                title,
                width,
                height,
                track_id,
                app_id: _,
            }) => {
                assert_eq!(title, "Doc A");
                assert_eq!((*width, *height), (800.0, 600.0));
                assert_eq!(track_id, "win-10");
            }
            other => panic!("frames[1] = {other:?}"),
        }
        assert!(matches!(
            &frames[2],
            DownFrame::Control(HostMessage::WindowOpened { window_id: 11, .. })
        ));
        assert!(matches!(
            &frames[3],
            DownFrame::Control(HostMessage::WindowMinimized { window_id: 11 })
        ));
        assert!(matches!(
            &frames[4],
            DownFrame::Control(HostMessage::InputMapping { window_id: 11, .. })
        ));
        assert_eq!(frames.len(), 5);
    }

    #[test]
    fn replay_skips_icon_when_app_has_none() {
        let state = AppState::for_test("NoIcon", Vec::new());
        assert!(replay_frames(&state).is_empty());
    }

    #[test]
    fn initial_icon_frame_only_when_icon_present() {
        let with_icon = AppState::for_test("Safari", vec![1u8, 2, 3]);
        match initial_icon_frame(&with_icon) {
            Some(DownFrame::Icon(png)) => assert_eq!(png, vec![1u8, 2, 3]),
            other => panic!("expected Some(Icon), got {other:?}"),
        }
        let without_icon = AppState::for_test("NoIcon", Vec::new());
        assert!(initial_icon_frame(&without_icon).is_none());
    }
}
