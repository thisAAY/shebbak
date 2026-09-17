use accessibility_sys::{
    kAXErrorSuccess, kAXFocusedWindowChangedNotification, kAXMenuClosedNotification,
    kAXMenuOpenedNotification, kAXSheetCreatedNotification, kAXTitleChangedNotification,
    kAXUIElementDestroyedNotification, kAXWindowCreatedNotification,
    kAXWindowDeminiaturizedNotification, kAXWindowMiniaturizedNotification,
    kAXWindowResizedNotification, AXObserverAddNotification, AXObserverCreate,
    AXObserverGetRunLoopSource, AXObserverRef, AXUIElementCreateApplication, AXUIElementRef,
};
use anyhow::{bail, Result};
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::runloop::{kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopSource};
use core_foundation::string::{CFString, CFStringRef};
use std::ffi::c_void;
use tracing::warn;

const NOTIFICATIONS: &[&str] = &[
    kAXWindowCreatedNotification,
    kAXUIElementDestroyedNotification,
    kAXSheetCreatedNotification,
    kAXMenuOpenedNotification,
    kAXMenuClosedNotification,
    kAXWindowMiniaturizedNotification,
    kAXWindowDeminiaturizedNotification,
    kAXTitleChangedNotification,
    kAXWindowResizedNotification,
    kAXFocusedWindowChangedNotification,
];

extern "C" fn observer_callback(
    _observer: AXObserverRef,
    _element: AXUIElementRef,
    _notification: CFStringRef,
    refcon: *mut c_void,
) {
    // SAFETY: refcon points at the Box<dyn Fn()> owned by the watcher thread's
    // closure environment, alive until CFRunLoopRun returns.
    let poke = unsafe { &*(refcon as *const Box<dyn Fn() + Send>) };
    poke();
}

/// CFRunLoop is only touched from its own thread except CFRunLoopStop, which
/// is documented thread-safe.
struct RunLoopHandle(CFRunLoop);
unsafe impl Send for RunLoopHandle {}

pub struct AxWatcher {
    runloop: RunLoopHandle,
    thread: std::thread::JoinHandle<()>,
}

impl AxWatcher {
    /// Spawns a dedicated CFRunLoop thread with one AXObserver per pid.
    /// `poke` is called (from that thread) on every subscribed notification.
    pub fn spawn(pids: Vec<i32>, poke: Box<dyn Fn() + Send>) -> Result<AxWatcher> {
        let (tx, rx) = std::sync::mpsc::channel::<RunLoopHandle>();
        let thread = std::thread::Builder::new().name("ax-watch".into()).spawn(move || {
            let poke: Box<dyn Fn() + Send> = poke; // owned by this frame
            let refcon = &poke as *const Box<dyn Fn() + Send> as *mut c_void;
            let mut retained: Vec<(AXObserverRef, AXUIElementRef)> = Vec::new();
            for pid in pids {
                unsafe {
                    let mut observer: AXObserverRef = std::ptr::null_mut();
                    if AXObserverCreate(pid, observer_callback, &mut observer) != kAXErrorSuccess {
                        warn!("AXObserverCreate failed for pid {pid}");
                        continue;
                    }
                    let app = AXUIElementCreateApplication(pid);
                    for name in NOTIFICATIONS {
                        let err = AXObserverAddNotification(
                            observer,
                            app,
                            CFString::from_static_string(name).as_concrete_TypeRef(),
                            refcon,
                        );
                        if err != kAXErrorSuccess {
                            // Some apps don't emit all notifications; log and continue.
                            warn!("AXObserverAddNotification({name}) pid {pid}: err {err}");
                        }
                    }
                    let source = AXObserverGetRunLoopSource(observer);
                    let source = CFRunLoopSource::wrap_under_get_rule(source);
                    CFRunLoop::get_current().add_source(&source, kCFRunLoopDefaultMode);
                    retained.push((observer, app));
                }
            }
            let _ = tx.send(RunLoopHandle(CFRunLoop::get_current()));
            CFRunLoop::run_current(); // blocks until stop()
            for (observer, app) in retained {
                unsafe {
                    CFRelease(observer as CFTypeRef);
                    CFRelease(app as CFTypeRef);
                }
            }
        })?;
        match rx.recv() {
            Ok(runloop) => Ok(AxWatcher { runloop, thread }),
            Err(_) => bail!("ax-watch thread died during setup"),
        }
    }

    /// Stops the run loop and joins the thread.
    ///
    /// `CFRunLoopStop` only stops a run loop that is currently running. There is a
    /// narrow window between `spawn()` sending the `RunLoopHandle` back and the
    /// watcher thread actually entering `CFRunLoopRun`; calling `stop()` inside that
    /// window is a no-op, and the `join()` below will then block forever. Do not
    /// call `stop()` synchronously right after `spawn()` returns — only at session
    /// teardown, once the watcher has had a chance to actually start running.
    pub fn stop(self) {
        self.runloop.0.stop();
        let _ = self.thread.join();
    }
}
