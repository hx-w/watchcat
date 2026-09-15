//! Cocoa notifications must be delivered on a live main run loop. Tokio and
//! provider I/O run on another thread; AX callbacks only schedule coalesced work.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_void};
use std::ptr::NonNull;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use block2::RcBlock;
use objc2::{
    MainThreadMarker,
    rc::{Retained, autoreleasepool},
    runtime::ProtocolObject,
};
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceDidActivateApplicationNotification,
    NSWorkspaceDidLaunchApplicationNotification, NSWorkspaceDidTerminateApplicationNotification,
    NSWorkspaceDidUnhideApplicationNotification, NSWorkspaceDidWakeNotification,
    NSWorkspaceSessionDidBecomeActiveNotification,
};
use objc2_application_services::{AXError, AXIsProcessTrusted, AXObserver, AXUIElement};
use objc2_core_foundation::Type;
use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFArray, CFBoolean, CFRetained, CFRunLoop, CFRunLoopSource,
    CFRunLoopSourceContext, CFRunLoopTimer, CFString, CFType, kCFRunLoopDefaultMode,
};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};
use tracing::{debug, info, warn};

use super::{is_allow_button, is_file_access_request, update_status};

static MAIN_RUNNING: AtomicBool = AtomicBool::new(false);
thread_local! {
    static MONITOR: RefCell<Option<Monitor>> = const { RefCell::new(None) };
    static QUEUED: RefCell<HashSet<i32>> = RefCell::new(HashSet::new());
}

fn on_main(task: impl Fn() + Send + 'static) {
    let Some(run_loop) = CFRunLoop::main() else {
        return;
    };
    let block = RcBlock::new(move || autoreleasepool(|_| task()));
    // CFRunLoopPerformBlock copies the block. Default mode is a CFString;
    // callers capture owned, Send data and touch Cocoa only inside the block.
    unsafe {
        run_loop.perform_block(Some(kCFRunLoopDefaultMode.unwrap().as_ref()), Some(&block));
    }
    run_loop.wake_up();
}

/// Keep AppKit on the process's main thread without creating an application,
/// a Dock icon, a second service, or a periodic timer.
pub fn run_main(task: impl FnOnce() -> Result<()> + Send + 'static) -> Result<()> {
    let _main_thread =
        MainThreadMarker::new().context("macOS event loop must run on the main thread")?;
    let run_loop = CFRunLoop::main().context("macOS main run loop is unavailable")?;
    // A dormant source keeps CFRunLoopRun alive before the daemon requests AX
    // subscriptions. It has no timer and does not consume CPU while idle.
    let mut context = CFRunLoopSourceContext {
        version: 0,
        info: std::ptr::null_mut(),
        retain: None,
        release: None,
        copyDescription: None,
        equal: None,
        hash: None,
        schedule: None,
        cancel: None,
        perform: None,
    };
    let source = unsafe { CFRunLoopSource::new(None, 0, &mut context) }
        .context("cannot create macOS event source")?;
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });
    MAIN_RUNNING.store(true, Ordering::Release);
    let worker = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task));
        on_main(|| {
            if let Some(run_loop) = CFRunLoop::main() {
                run_loop.stop();
            }
        });
        result
    });
    CFRunLoop::run();
    MAIN_RUNNING.store(false, Ordering::Release);
    MONITOR.with(|monitor| monitor.borrow_mut().take());
    source.invalidate();
    match worker.join() {
        Ok(Ok(result)) => result,
        Ok(Err(panic)) | Err(panic) => std::panic::resume_unwind(panic),
    }
}

pub struct Guard {
    permit: Arc<Mutex<bool>>,
}

impl Guard {
    pub fn stop(&self) {
        // Synchronize with the actual press, not just queued callbacks. Once
        // stop returns, no new click can cross this boundary.
        *self.permit.lock().unwrap_or_else(|e| e.into_inner()) = false;
        if MAIN_RUNNING.load(Ordering::Acquire) {
            on_main(|| {
                MONITOR.with(|monitor| monitor.borrow_mut().take());
                update_status(|status| status.state = "stopped".into());
            });
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start(dry_run: bool) -> Guard {
    let permit = Arc::new(Mutex::new(true));
    if MAIN_RUNNING.load(Ordering::Acquire) {
        let permission = Arc::clone(&permit);
        update_status(|status| status.state = "starting".into());
        on_main(move || {
            if !*permission.lock().unwrap_or_else(|e| e.into_inner()) {
                return;
            }
            MONITOR.with(|monitor| {
                *monitor.borrow_mut() = Some(Monitor::new(Arc::clone(&permission), dry_run));
            });
            queue_scan(0);
        });
    } else {
        update_status(|status| status.state = "no_desktop_event_loop".into());
    }
    Guard { permit }
}

// PID 0 is an event-triggered reconciliation of application lifetimes.
fn queue_scan(pid: i32) {
    if !QUEUED.with(|queued| queued.borrow_mut().insert(pid)) {
        return;
    }
    on_main(move || {
        QUEUED.with(|queued| queued.borrow_mut().remove(&pid));
        autoreleasepool(|_| {
            MONITOR.with(|monitor| {
                if let Some(monitor) = monitor.borrow_mut().as_mut() {
                    if !*monitor.permit.lock().unwrap_or_else(|e| e.into_inner()) {
                        return;
                    }
                    if pid == 0 {
                        monitor.reconcile();
                    } else {
                        monitor.scan(pid);
                    }
                }
            })
        });
    });
}

unsafe extern "C-unwind" fn ax_event(
    _observer: NonNull<AXObserver>,
    _element: NonNull<AXUIElement>,
    _notification: NonNull<CFString>,
    context: *mut c_void,
) {
    // The context is an integer PID, never dereferenced.
    queue_scan(context as isize as i32);
}

struct Monitor {
    center: Retained<NSNotificationCenter>,
    tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
    processes: HashMap<i32, Process>,
    permit: Arc<Mutex<bool>>,
    dry_run: bool,
}

impl Monitor {
    fn new(permit: Arc<Mutex<bool>>, dry_run: bool) -> Self {
        // On an application element, this setting affects that element only.
        // Setting it on the system-wide element bounds calls on descendants too.
        unsafe {
            AXUIElement::new_system_wide().set_messaging_timeout(0.25);
        }
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let mut tokens = Vec::new();
        // Launch/unhide/activation also cover a host whose AX server was not
        // ready at launch. Wake/session events re-establish dropped observers.
        for (name, reset) in unsafe {
            [
                (NSWorkspaceDidLaunchApplicationNotification, false),
                (NSWorkspaceDidTerminateApplicationNotification, false),
                (NSWorkspaceDidActivateApplicationNotification, false),
                (NSWorkspaceDidUnhideApplicationNotification, false),
                (NSWorkspaceDidWakeNotification, true),
                (NSWorkspaceSessionDidBecomeActiveNotification, true),
            ]
        } {
            let callback = RcBlock::new(move |_: NonNull<NSNotification>| {
                on_main(move || {
                    if reset {
                        MONITOR.with(|monitor| {
                            if let Some(monitor) = monitor.borrow_mut().as_mut() {
                                monitor.reconnect();
                            }
                        });
                    }
                    queue_scan(0);
                });
            });
            // No object filter or operation queue; the callback captures only
            // a bool and forwards work onto the main run loop.
            tokens.push(unsafe {
                center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &callback)
            });
        }
        Self {
            center,
            tokens,
            processes: HashMap::new(),
            permit,
            dry_run,
        }
    }

    fn reconcile(&mut self) {
        // This never requests or auto-grants Watchcat's own Accessibility
        // permission. Grant once in Settings; activation then retries setup.
        if !unsafe { AXIsProcessTrusted() } {
            self.processes.clear();
            let previous = super::status().state;
            update_status(|status| {
                status.state = "accessibility_required".into();
                status.listening_processes = 0;
                status.last_result = std::env::current_exe().ok().map(|path| {
                    format!(
                        "Enable Accessibility for {} and restart the service",
                        path.display()
                    )
                });
            });
            if previous != "accessibility_required" {
                warn!(
                    "OS dialog handling needs Accessibility permission for watchcatd; grant it in System Settings > Privacy & Security > Accessibility, then restart the service"
                );
            }
            return;
        }
        self.processes.retain(|_, process| {
            if !process.app.isTerminated() {
                return true;
            }
            if process.dialogs.iter().any(|dialog| dialog.awaiting_close) {
                update_status(|status| {
                    status.last_result =
                        Some("System UI host exited after Allow; result unconfirmed".into())
                });
            }
            false
        });
        let mut incomplete = false;
        for app in NSWorkspace::sharedWorkspace()
            .runningApplications()
            .to_vec()
        {
            let pid = app.processIdentifier();
            if self.processes.contains_key(&pid) || !is_system_host(pid) {
                continue;
            }
            match Process::new(app) {
                Ok(process) => {
                    self.processes.insert(pid, process);
                }
                Err(error) => {
                    incomplete = true;
                    debug!(pid, ?error, "system UI host cannot be observed");
                }
            }
        }
        update_status(|status| {
            status.listening_processes = self.processes.len();
            status.state = if incomplete {
                "partial"
            } else if self.dry_run {
                "dry_run"
            } else if self.processes.is_empty() {
                "waiting_for_system_ui"
            } else {
                "listening"
            }
            .into();
        });
        // Subscribe first, then inspect existing windows once. Subsequent work
        // is exclusively driven by Cocoa/AX events or a click's one-shot deadline.
        for &pid in self.processes.keys() {
            queue_scan(pid);
        }
    }

    fn reconnect(&mut self) {
        for process in self.processes.values_mut() {
            if let Ok(mut replacement) = Process::new(process.app.clone()) {
                // Preserve attempted requests across sleep/session changes:
                // reconnecting an observer must not authorize a second press.
                replacement.dialogs = std::mem::take(&mut process.dialogs);
                for dialog in &replacement.dialogs {
                    watch_dialog(
                        &replacement.observer,
                        &dialog.element,
                        replacement.app.processIdentifier(),
                    );
                }
                *process = replacement;
            }
        }
    }

    fn scan(&mut self, pid: i32) {
        if !unsafe { AXIsProcessTrusted() } {
            self.reconcile();
            return;
        }
        let Some(process) = self.processes.get_mut(&pid) else {
            return;
        };
        if process.app.isTerminated() || !is_system_host(pid) {
            return;
        }
        if let Err(error) = process.scan(&self.permit, self.dry_run) {
            update_status(|status| {
                status.state = "partial".into();
                status.last_result = Some(format!("Cannot read system dialog: {error:?}"));
            });
            debug!(pid, ?error, "cannot inspect system dialogs");
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        for token in &self.tokens {
            unsafe {
                self.center.removeObserver((**token).as_ref());
            }
        }
        self.processes.clear();
    }
}

struct Dialog {
    element: CFRetained<AXUIElement>,
    last_attempt: Option<String>,
    awaiting_close: bool,
    deadline: Option<Instant>,
    timer: Option<CFRetained<CFRunLoopTimer>>,
}

impl Drop for Dialog {
    fn drop(&mut self) {
        if let Some(timer) = &self.timer {
            timer.invalidate();
        }
    }
}

struct Process {
    app: Retained<NSRunningApplication>,
    element: CFRetained<AXUIElement>,
    observer: CFRetained<AXObserver>,
    dialogs: Vec<Dialog>,
}

impl Process {
    fn new(app: Retained<NSRunningApplication>) -> std::result::Result<Self, AXError> {
        let pid = app.processIdentifier();
        let element = unsafe { AXUIElement::new_application(pid) };
        unsafe {
            element.set_messaging_timeout(0.25);
        }
        let mut pointer = std::ptr::null_mut();
        let result =
            unsafe { AXObserver::create(pid, Some(ax_event), NonNull::from(&mut pointer)) };
        if result != AXError::Success {
            return Err(result);
        }
        let observer =
            unsafe { CFRetained::from_raw(NonNull::new(pointer).ok_or(AXError::Failure)?) };
        let mut subscribed = false;
        for name in [
            "AXWindowCreated",
            "AXSheetCreated",
            "AXFocusedWindowChanged",
        ] {
            subscribed |= subscribe(&observer, &element, name, pid);
        }
        if !subscribed {
            return Err(AXError::NotificationUnsupported);
        }
        let source = unsafe { observer.run_loop_source() };
        CFRunLoop::main()
            .unwrap()
            .add_source(Some(&source), unsafe { kCFRunLoopDefaultMode });
        info!(pid, "subscribed to system dialog events");
        Ok(Self {
            app,
            element,
            observer,
            dialogs: Vec::new(),
        })
    }

    fn scan(&mut self, permit: &Mutex<bool>, dry_run: bool) -> std::result::Result<(), AXError> {
        let windows = elements(&self.element, "AXWindows")?;
        // Only a successful read can prove a previously observed dialog absent.
        self.dialogs.retain(|dialog| {
            let present = windows.iter().any(|window| window == &dialog.element);
            if !present && dialog.awaiting_close {
                update_status(|status| {
                    status.dialogs_closed += 1;
                    status.last_result = Some("Dialog closed after Allow".into());
                });
                info!(
                    pid = self.app.processIdentifier(),
                    "permission dialog closed after Allow"
                );
            }
            present
        });
        for window in windows.into_iter().take(32) {
            if !*permit.lock().unwrap_or_else(|e| e.into_inner()) {
                return Ok(());
            }
            let index = match self
                .dialogs
                .iter()
                .position(|dialog| dialog.element == window)
            {
                Some(index) => index,
                None => {
                    let pid = self.app.processIdentifier();
                    watch_dialog(&self.observer, &window, pid);
                    self.dialogs.push(Dialog {
                        element: window,
                        last_attempt: None,
                        awaiting_close: false,
                        deadline: None,
                        timer: None,
                    });
                    self.dialogs.len() - 1
                }
            };
            let dialog = &mut self.dialogs[index];
            if dialog.awaiting_close
                && dialog
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
            {
                dialog.awaiting_close = false;
                dialog.timer.take();
                update_status(|status| {
                    status.last_result =
                        Some("Allow sent; dialog still present (not retried)".into())
                });
                warn!(
                    pid = self.app.processIdentifier(),
                    "Allow sent but permission dialog is still present"
                );
            }
            let Some((button, request)) = allow_button(&dialog.element, permit) else {
                continue;
            };
            // System hosts can reuse a window for the next permission request.
            // Deduplicate the request, not the lifetime of its outer window.
            if dialog.last_attempt.as_ref() == Some(&request) {
                continue;
            }
            if dry_run {
                dialog.last_attempt = Some(request);
                update_status(|status| {
                    status.last_result =
                        Some("Would allow a directory access request (dry run)".into())
                });
                info!(
                    pid = self.app.processIdentifier(),
                    "would allow directory access (dry run)"
                );
                continue;
            }
            // Re-read the target immediately before pressing it. Never use a
            // default button, a global keystroke, or a cached screen coordinate.
            let Some((current_button, current_request)) = allow_button(&dialog.element, permit)
            else {
                continue;
            };
            if button != current_button
                || request != current_request
                || self.app.isTerminated()
                || !is_system_host(self.app.processIdentifier())
            {
                continue;
            }
            let allowed = permit.lock().unwrap_or_else(|e| e.into_inner());
            if !*allowed {
                return Ok(());
            }
            dialog.last_attempt = Some(request);
            dialog.awaiting_close = false;
            if let Some(timer) = dialog.timer.take() {
                timer.invalidate();
            }
            let result = unsafe { current_button.perform_action(&CFString::from_str("AXPress")) };
            drop(allowed);
            // CannotComplete may mean the target processed the press but did
            // not acknowledge it. Do not retry an ambiguous action.
            if result == AXError::Success || result == AXError::CannotComplete {
                dialog.awaiting_close = true;
                dialog.deadline = Some(Instant::now() + Duration::from_secs(3));
                dialog.timer = verification_timer(self.app.processIdentifier());
                update_status(|status| {
                    status.clicks_sent += 1;
                    status.last_result = Some("Allow sent; waiting for dialog to close".into());
                });
                info!(
                    pid = self.app.processIdentifier(),
                    ?result,
                    "pressed Allow on a directory access request"
                );
                queue_scan(self.app.processIdentifier());
            } else {
                update_status(|status| {
                    status.last_result = Some(format!("Allow failed: {result:?}"))
                });
                warn!(
                    pid = self.app.processIdentifier(),
                    ?result,
                    "cannot press Allow on permission dialog"
                );
            }
        }
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Releasing an observer removes its run loop source; explicitly remove
        // it as well while the main loop and process are still alive.
        if let Some(run_loop) = CFRunLoop::main() {
            let source = unsafe { self.observer.run_loop_source() };
            run_loop.remove_source(Some(&source), unsafe { kCFRunLoopDefaultMode });
        }
    }
}

fn subscribe(observer: &AXObserver, element: &AXUIElement, name: &str, pid: i32) -> bool {
    let result = unsafe {
        observer.add_notification(
            element,
            &CFString::from_str(name),
            pid as isize as *mut c_void,
        )
    };
    matches!(
        result,
        AXError::Success | AXError::NotificationAlreadyRegistered
    )
}

fn watch_dialog(observer: &AXObserver, element: &AXUIElement, pid: i32) {
    for name in ["AXUIElementDestroyed", "AXLayoutChanged", "AXSheetCreated"] {
        subscribe(observer, element, name, pid);
    }
}

fn verification_timer(pid: i32) -> Option<CFRetained<CFRunLoopTimer>> {
    let callback = RcBlock::new(move |_: *mut CFRunLoopTimer| queue_scan(pid));
    // Interval zero makes this a one-shot deadline, never a periodic scan.
    let timer = unsafe {
        CFRunLoopTimer::with_handler(
            None,
            CFAbsoluteTimeGetCurrent() + 3.0,
            0.0,
            0,
            0,
            Some(&callback),
        )
    }?;
    CFRunLoop::main()?.add_timer(Some(&timer), unsafe { kCFRunLoopDefaultMode });
    Some(timer)
}

fn attribute(
    element: &AXUIElement,
    name: &str,
) -> std::result::Result<CFRetained<CFType>, AXError> {
    let mut value = std::ptr::null();
    let result = unsafe {
        element.copy_attribute_value(&CFString::from_str(name), NonNull::from(&mut value))
    };
    if result != AXError::Success {
        return Err(result);
    }
    let value = NonNull::new(value.cast_mut()).ok_or(AXError::NoValue)?;
    Ok(unsafe { CFRetained::from_raw(value) })
}

fn string(element: &AXUIElement, name: &str) -> std::result::Result<String, AXError> {
    match attribute(element, name) {
        Ok(value) => Ok(value
            .downcast::<CFString>()
            .ok()
            .map(|value| value.to_string())
            .unwrap_or_default()),
        Err(AXError::AttributeUnsupported | AXError::NoValue) => Ok(String::new()),
        Err(error) => Err(error),
    }
}

fn elements(
    element: &AXUIElement,
    name: &str,
) -> std::result::Result<Vec<CFRetained<AXUIElement>>, AXError> {
    let array = attribute(element, name)?
        .downcast::<CFArray>()
        .map_err(|_| AXError::Failure)?;
    // AX array attributes contain CF objects; retain and type-check each child.
    let array = unsafe { CFRetained::cast_unchecked::<CFArray<CFType>>(array) };
    Ok(array
        .to_vec()
        .into_iter()
        .filter_map(|value| value.downcast::<AXUIElement>().ok())
        .collect())
}

fn allow_button(
    window: &AXUIElement,
    permit: &Mutex<bool>,
) -> Option<(CFRetained<AXUIElement>, String)> {
    let mut stack = vec![(window.retain(), 0)];
    let mut text = String::new();
    let mut heading = None;
    let mut buttons = Vec::new();
    let mut visited = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    while let Some((element, depth)) = stack.pop() {
        if !*permit.lock().unwrap_or_else(|e| e.into_inner())
            || visited.len() >= 256
            || depth > 20
            || Instant::now() >= deadline
        {
            return None;
        }
        if !visited.insert(CFRetained::as_ptr(&element).as_ptr() as usize) {
            continue;
        }
        let role = string(&element, "AXRole").ok()?;
        if matches!(role.as_str(), "AXTextField" | "AXTextArea") {
            return None;
        }
        if role == "AXButton" {
            let title = string(&element, "AXTitle").ok()?;
            let label = if title.is_empty() {
                string(&element, "AXDescription").ok()?
            } else {
                title
            };
            if is_allow_button(&label) {
                let enabled = attribute(&element, "AXEnabled")
                    .ok()?
                    .downcast::<CFBoolean>()
                    .ok()?;
                if enabled.value() {
                    buttons.push(element.clone());
                }
            }
        } else if matches!(role.as_str(), "AXStaticText" | "AXHeading") {
            // AX children are visited in source order. The primary text is the
            // consent heading; supplementary usage descriptions, container
            // labels, and icon descriptions must never authorize a press.
            let value = string(&element, "AXValue").ok()?;
            let value = if value.trim().is_empty() {
                string(&element, "AXTitle").ok()?
            } else {
                value
            };
            if !value.trim().is_empty() {
                heading.get_or_insert_with(|| value.clone());
                text.push_str(&value);
                text.push('\n');
            }
        }
        if text.len() > 16384 {
            return None;
        }
        match elements(&element, "AXChildren") {
            Ok(children) => {
                stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)))
            }
            Err(AXError::AttributeUnsupported | AXError::NoValue) => {}
            Err(_) => return None,
        }
    }
    if heading.as_deref().is_some_and(is_file_access_request) && buttons.len() == 1 {
        buttons.pop().map(|button| (button, text))
    } else {
        None
    }
}

// Read the real executable path from the kernel, not a display name or bundle
// identifier supplied by an application. UI hosts must live on the system
// volume. Arbitrary application windows never reach the consent matcher.
fn is_system_host(pid: i32) -> bool {
    unsafe extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
    }
    let mut path = [0_i8; 4096];
    let length = unsafe { proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if length <= 0 {
        return false;
    }
    let path = unsafe { CStr::from_ptr(path.as_ptr()) }.to_string_lossy();
    path.starts_with("/System/Library/")
}
