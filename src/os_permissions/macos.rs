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
    AnyThread, MainThreadMarker, define_class, msg_send,
    rc::{Retained, autoreleasepool},
    runtime::{AnyObject, ProtocolObject},
};
use objc2_app_kit::{
    NSRunningApplication, NSWorkspace, NSWorkspaceApplicationKey,
    NSWorkspaceDidActivateApplicationNotification, NSWorkspaceDidLaunchApplicationNotification,
    NSWorkspaceDidTerminateApplicationNotification, NSWorkspaceDidUnhideApplicationNotification,
    NSWorkspaceDidWakeNotification, NSWorkspaceSessionDidBecomeActiveNotification,
};
use objc2_application_services::{AXError, AXIsProcessTrusted, AXObserver, AXUIElement};
use objc2_core_foundation::Type;
use objc2_core_foundation::{
    CFAbsoluteTimeGetCurrent, CFArray, CFBoolean, CFRetained, CFRunLoop, CFRunLoopSource,
    CFRunLoopSourceContext, CFRunLoopTimer, CFString, CFType, kCFRunLoopDefaultMode,
};
use objc2_foundation::{
    NSDictionary, NSKeyValueChangeKey, NSKeyValueObservingOptions, NSNotification,
    NSNotificationCenter, NSObject, NSObjectNSKeyValueObserverRegistration, NSObjectProtocol,
    NSString, ns_string,
};
use tracing::{debug, info, warn};

use super::rules::{DIRECTORY_RULE, DialogSettings};
use super::{RULES, is_allow_button, record, update_status};

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

// NSWorkspace launch notifications omit background/LSUIElement applications.
// KVO on runningApplications is the documented subscription for those hosts.
define_class!(
    #[unsafe(super(NSObject))]
    struct ApplicationsObserver;
    impl ApplicationsObserver {
        #[unsafe(method(observeValueForKeyPath:ofObject:change:context:))]
        fn changed(&self, _key: Option<&NSString>, _object: Option<&AnyObject>,
            _change: Option<&NSDictionary<NSKeyValueChangeKey, AnyObject>>, _context: *mut c_void) {
            // No Cocoa object crosses threads; enqueue before touching monitor state.
            #[cfg(test)]
            KVO_DELIVERIES.fetch_add(1, Ordering::SeqCst);
            if MAIN_RUNNING.load(Ordering::Acquire) { on_main(|| queue_scan(0)); }
        }
    }
    unsafe impl NSObjectProtocol for ApplicationsObserver {}
);

pub(super) fn configuration_changed() {
    if MAIN_RUNNING.load(Ordering::Acquire) {
        on_main(|| {
            MONITOR.with(|monitor| {
                if let Some(monitor) = monitor.borrow_mut().as_mut() {
                    monitor.failed.clear();
                    for process in monitor.processes.values_mut() {
                        for dialog in &mut process.dialogs {
                            dialog.last_observation = None;
                        }
                    }
                }
            });
            queue_scan(0);
        });
    }
}

struct PendingProcess {
    app: Retained<NSRunningApplication>,
    attempts: usize,
    timer: Option<CFRetained<CFRunLoopTimer>>,
}
impl Drop for PendingProcess {
    fn drop(&mut self) {
        if let Some(timer) = &self.timer {
            timer.invalidate();
        }
    }
}

struct Monitor {
    workspace: Retained<NSWorkspace>,
    applications_observer: Retained<ApplicationsObserver>,
    failed: HashMap<i32, PendingProcess>,
    initialized: bool,
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
        let workspace = NSWorkspace::sharedWorkspace();
        let applications_observer: Retained<ApplicationsObserver> =
            unsafe { msg_send![ApplicationsObserver::alloc(), init] };
        unsafe {
            workspace.addObserver_forKeyPath_options_context(
                &applications_observer,
                ns_string!("runningApplications"),
                NSKeyValueObservingOptions::empty(),
                std::ptr::null_mut(),
            );
        }
        let center = workspace.notificationCenter();
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
            let callback = RcBlock::new(move |notification: NonNull<NSNotification>| {
                let pid = unsafe { notification.as_ref() }
                    .userInfo()
                    .and_then(|info| {
                        info.objectForKey(unsafe { NSWorkspaceApplicationKey }.as_ref())
                    })
                    .and_then(|app| app.downcast::<NSRunningApplication>().ok())
                    .map(|app| app.processIdentifier());
                on_main(move || {
                    MONITOR.with(|monitor| {
                        if let Some(monitor) = monitor.borrow_mut().as_mut() {
                            reset_failed(&mut monitor.failed, pid, reset);
                            if reset {
                                monitor.reconnect();
                            }
                        }
                    });
                    queue_scan(0);
                });
            });
            // Copy only the PID and reset flag onto the main run loop.
            tokens.push(unsafe {
                center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &callback)
            });
        }
        Self {
            workspace,
            applications_observer,
            failed: HashMap::new(),
            initialized: false,
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
            self.failed.clear();
            let previous = super::status().state;
            update_status(|status| {
                status.state = "accessibility_required".into();
                status.listening_processes = 0;
                status.unavailable_processes = 0;
                status.last_result = std::env::current_exe().ok().map(|path| {
                    format!(
                        "Enable Accessibility for {} and restart the service",
                        path.display()
                    )
                });
            });
            if previous != "accessibility_required" {
                record(
                    "monitor.permission_required",
                    None,
                    "watchcatd",
                    None,
                    None,
                    "Grant Accessibility to watchcatd and restart",
                );
                warn!(
                    "OS dialog handling needs Accessibility permission for watchcatd; grant it in System Settings > Privacy & Security > Accessibility, then restart the service"
                );
            }
            return;
        }
        let rules = RULES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();
        self.processes.retain(|&pid, process| {
            let keep = !process.app.isTerminated()
                && app_identity(&process.app)
                    .is_some_and(|(path, bundle)| rules.observes(&path, &bundle));
            if !keep {
                record(
                    "process.detached",
                    Some(pid),
                    &app_name(&process.app),
                    None,
                    None,
                    "process exited or no enabled rule applies",
                );
                for dialog in &process.dialogs {
                    if dialog.awaiting_close {
                        record(
                            "click.unconfirmed",
                            Some(pid),
                            &app_name(&process.app),
                            dialog.last_rule.as_deref(),
                            dialog.last_button.as_deref(),
                            "host exited or rule removed before close was confirmed",
                        );
                    }
                }
            }
            keep
        });
        self.failed.retain(|_, pending| {
            !pending.app.isTerminated()
                && app_identity(&pending.app)
                    .is_some_and(|(path, bundle)| rules.observes(&path, &bundle))
        });
        for app in self.workspace.runningApplications().to_vec() {
            if !*self.permit.lock().unwrap_or_else(|e| e.into_inner()) {
                return;
            }
            let pid = app.processIdentifier();
            if self.processes.contains_key(&pid)
                || self.failed.contains_key(&pid)
                || app.isTerminated()
                || !app_identity(&app).is_some_and(|(path, bundle)| rules.observes(&path, &bundle))
            {
                continue;
            }
            record(
                "process.discovered",
                Some(pid),
                &app_name(&app),
                None,
                None,
                if self.initialized {
                    "new process from application-list event; installing window observer"
                } else {
                    "initial application snapshot; installing window observer"
                },
            );
            self.attach(app, 0);
        }
        self.initialized = true;
        let incomplete = !self.failed.is_empty();
        update_status(|status| {
            status.listening_processes = self.processes.len();
            status.unavailable_processes = self.failed.len();
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

    fn attach(&mut self, app: Retained<NSRunningApplication>, previous_attempts: usize) {
        let pid = app.processIdentifier();
        match Process::new(app.clone()) {
            Ok(process) => {
                self.failed.remove(&pid);
                record(
                    "process.observed",
                    Some(pid),
                    &app_name(&app),
                    None,
                    None,
                    &format!(
                        "subscribed: {}; initial window check",
                        process.subscriptions.join(", ")
                    ),
                );
                self.processes.insert(pid, process);
                queue_scan(pid);
            }
            Err(error) => {
                let attempts = previous_attempts + 1;
                let timer = if attempts < 4
                    && matches!(error, AXError::CannotComplete | AXError::InvalidUIElement)
                {
                    let callback = RcBlock::new(move |_: *mut CFRunLoopTimer| {
                        on_main(move || {
                            MONITOR.with(|monitor| {
                                if let Some(monitor) = monitor.borrow_mut().as_mut() {
                                    if let Some(pending) = monitor.failed.remove(&pid) {
                                        if !pending.app.isTerminated()
                                            && *monitor
                                                .permit
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                        {
                                            monitor.attach(pending.app.clone(), pending.attempts);
                                        }
                                    }
                                }
                            });
                            queue_scan(0);
                        });
                    });
                    let timer = unsafe {
                        CFRunLoopTimer::with_handler(
                            None,
                            CFAbsoluteTimeGetCurrent() + [0.25, 1.0, 3.0][attempts - 1],
                            0.0,
                            0,
                            0,
                            Some(&callback),
                        )
                    };
                    if let Some(timer) = &timer {
                        CFRunLoop::main()
                            .unwrap()
                            .add_timer(Some(timer), unsafe { kCFRunLoopDefaultMode });
                    }
                    timer
                } else {
                    None
                };
                record(
                    if timer.is_some() {
                        "process.retry"
                    } else {
                        "process.unavailable"
                    },
                    Some(pid),
                    &app_name(&app),
                    None,
                    None,
                    &format!("AX subscription attempt {attempts}/4: {error:?}"),
                );
                self.failed.insert(
                    pid,
                    PendingProcess {
                        app,
                        attempts,
                        timer,
                    },
                );
            }
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
        if process.app.isTerminated() {
            return;
        }
        if let Err(error) = process.scan(&self.permit, self.dry_run) {
            if process.last_error != Some(error) {
                record(
                    "process.read_failed",
                    Some(pid),
                    &app_name(&process.app),
                    None,
                    None,
                    &format!("AXWindows: {error:?}"),
                );
                process.last_error = Some(error);
            }
            update_status(|status| {
                status.state = "partial".into();
                status.last_result = Some(format!("Cannot read system dialog: {error:?}"));
            });
            debug!(pid, ?error, "cannot inspect system dialogs");
        } else {
            process.last_error = None;
        }
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        unsafe {
            self.workspace.removeObserver_forKeyPath(
                &self.applications_observer,
                ns_string!("runningApplications"),
            );
        }
        self.failed.clear();
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
    last_observation: Option<String>,
    last_rule: Option<String>,
    last_button: Option<String>,
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
    subscriptions: Vec<String>,
    last_error: Option<AXError>,
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
        let subscriptions =
            process_subscriptions(|name| subscribe(&observer, &element, name, pid))?;
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
            subscriptions,
            last_error: None,
        })
    }

    fn scan(&mut self, permit: &Mutex<bool>, dry_run: bool) -> std::result::Result<(), AXError> {
        let pid = self.app.processIdentifier();
        let app = app_name(&self.app);
        let windows = elements(&self.element, "AXWindows")?;
        self.dialogs.retain(|dialog| {
            let present = windows.iter().any(|window| window == &dialog.element);
            if !present && dialog.awaiting_close {
                record(
                    "click.closed",
                    Some(pid),
                    &app,
                    dialog.last_rule.as_deref(),
                    dialog.last_button.as_deref(),
                    "window absent after click; original tool result is unknown",
                );
                update_status(|status| {
                    status.dialogs_closed += 1;
                    status.last_result = Some("Dialog closed after click".into());
                });
            }
            present
        });
        for window in windows.into_iter().take(32) {
            if !*permit.lock().unwrap_or_else(|e| e.into_inner()) {
                return Ok(());
            }
            // A config update waits for any in-flight press; when it returns,
            // no removed/disabled rule can authorize a subsequent action.
            let rules_guard = RULES.lock().unwrap_or_else(|e| e.into_inner());
            let Some(rules) = rules_guard.as_ref() else {
                return Ok(());
            };
            let Some(identity) = app_identity(&self.app) else {
                return Ok(());
            };
            let index = match self
                .dialogs
                .iter()
                .position(|dialog| dialog.element == window)
            {
                Some(index) => index,
                None => {
                    watch_dialog(&self.observer, &window, pid);
                    record(
                        "window.discovered",
                        Some(pid),
                        &app,
                        None,
                        None,
                        "window event or initial window snapshot",
                    );
                    self.dialogs.push(Dialog {
                        element: window,
                        last_attempt: None,
                        last_observation: None,
                        last_rule: None,
                        last_button: None,
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
                record(
                    "click.unconfirmed",
                    Some(pid),
                    &app,
                    dialog.last_rule.as_deref(),
                    dialog.last_button.as_deref(),
                    "window still present after 3 seconds; click will not be retried",
                );
                update_status(|status| {
                    status.last_result =
                        Some("Click sent; window still present (not retried)".into())
                });
            }
            let matched = match matching_button(&dialog.element, permit, rules, &identity) {
                Ok(matched) => matched,
                Err(reason) => {
                    if dialog.last_observation.as_deref() != Some(reason) {
                        record("window.skipped", Some(pid), &app, None, None, reason);
                        dialog.last_observation = Some(reason.into());
                    }
                    continue;
                }
            };
            if dialog.last_attempt.as_ref() == Some(&matched.signature) {
                continue;
            }
            dialog.last_observation = Some(format!("matched {}", matched.rule));
            if dry_run {
                record(
                    "click.dry_run",
                    Some(pid),
                    &app,
                    Some(&matched.rule),
                    Some(&matched.label),
                    "rule matched; no click in dry run",
                );
                dialog.last_attempt = Some(matched.signature);
                continue;
            }
            let Ok(current) = matching_button(&dialog.element, permit, rules, &identity) else {
                continue;
            };
            if matched.button != current.button
                || matched.signature != current.signature
                || matched.rule != current.rule
                || self.app.isTerminated()
                || app_identity(&self.app).as_ref() != Some(&identity)
            {
                continue;
            }
            let allowed = permit.lock().unwrap_or_else(|e| e.into_inner());
            if !*allowed {
                return Ok(());
            }
            // Persist intent before mutation; no unaudited press when storage fails.
            if !record(
                "click.attempt",
                Some(pid),
                &app,
                Some(&matched.rule),
                Some(&matched.label),
                "matched rule; sending AXPress",
            ) {
                update_status(|status| {
                    status.last_result = Some("Click skipped: cannot write event log".into())
                });
                continue;
            }
            dialog.last_attempt = Some(matched.signature);
            dialog.last_rule = Some(matched.rule.clone());
            dialog.last_button = Some(matched.label.clone());
            dialog.awaiting_close = false;
            if let Some(timer) = dialog.timer.take() {
                timer.invalidate();
            }
            let result = unsafe {
                current
                    .button
                    .perform_action(&CFString::from_str("AXPress"))
            };
            drop(allowed);
            if result == AXError::Success || result == AXError::CannotComplete {
                dialog.awaiting_close = true;
                dialog.deadline = Some(Instant::now() + Duration::from_secs(3));
                dialog.timer = verification_timer(pid);
                record(
                    "click.sent",
                    Some(pid),
                    &app,
                    Some(&matched.rule),
                    Some(&matched.label),
                    &format!("AXPress: {result:?}; waiting for window to close"),
                );
                update_status(|status| {
                    status.clicks_sent += 1;
                    status.last_result = Some(format!(
                        "Clicked {} via {}; waiting for window to close",
                        matched.label, matched.rule
                    ));
                });
                info!(
                    pid,
                    rule = matched.rule,
                    button = matched.label,
                    ?result,
                    "automatic dialog click sent"
                );
                queue_scan(pid);
            } else {
                record(
                    "click.failed",
                    Some(pid),
                    &app,
                    Some(&matched.rule),
                    Some(&matched.label),
                    &format!("AXPress: {result:?}; not retried"),
                );
                update_status(|status| {
                    status.last_result = Some(format!("Click failed: {result:?}"))
                });
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

fn subscribe(observer: &AXObserver, element: &AXUIElement, name: &str, pid: i32) -> AXError {
    unsafe {
        observer.add_notification(
            element,
            &CFString::from_str(name),
            pid as isize as *mut c_void,
        )
    }
}

fn reset_failed<T>(failed: &mut HashMap<i32, T>, pid: Option<i32>, reset: bool) {
    if reset {
        failed.clear();
    } else if let Some(pid) = pid {
        failed.remove(&pid);
    }
}

fn process_subscriptions(
    mut add: impl FnMut(&str) -> AXError,
) -> std::result::Result<Vec<String>, AXError> {
    let mut subscriptions = Vec::new();
    let mut first_error = None;
    let mut transient_error = None;
    for name in [
        "AXWindowCreated",
        "AXSheetCreated",
        "AXFocusedWindowChanged",
    ] {
        match add(name) {
            AXError::Success | AXError::NotificationAlreadyRegistered => {
                subscriptions.push(name.into())
            }
            error @ (AXError::CannotComplete | AXError::InvalidUIElement) => {
                transient_error.get_or_insert(error);
            }
            error => {
                first_error.get_or_insert(error);
            }
        }
    }
    // A successful focus subscription must not hide a window subscription
    // that failed while a newly launched app's AX server was still starting.
    if let Some(error) = transient_error {
        Err(error)
    } else if subscriptions.is_empty() {
        Err(first_error.unwrap_or(AXError::NotificationUnsupported))
    } else {
        Ok(subscriptions)
    }
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

struct MatchedButton {
    button: CFRetained<AXUIElement>,
    signature: String,
    rule: String,
    label: String,
}

fn matching_button(
    window: &AXUIElement,
    permit: &Mutex<bool>,
    rules: &DialogSettings,
    identity: &(String, String),
) -> std::result::Result<MatchedButton, &'static str> {
    let title = string(window, "AXTitle").map_err(|_| "cannot read window title")?;
    let mut stack = vec![(window.retain(), 0)];
    let mut text = String::new();
    let mut heading = None;
    let mut buttons = Vec::new();
    let mut visited = HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    while let Some((element, depth)) = stack.pop() {
        if !*permit.lock().unwrap_or_else(|e| e.into_inner()) {
            return Err("monitor stopped");
        }
        if visited.len() >= 256 || depth > 20 || Instant::now() >= deadline {
            return Err("window inspection budget exceeded");
        }
        if !visited.insert(CFRetained::as_ptr(&element).as_ptr() as usize) {
            continue;
        }
        let role = string(&element, "AXRole").map_err(|_| "cannot read element role")?;
        if matches!(role.as_str(), "AXTextField" | "AXTextArea") {
            return Err("window contains editable or credential fields");
        }
        if role == "AXButton" {
            let title = string(&element, "AXTitle").map_err(|_| "cannot read button title")?;
            let label = if title.is_empty() {
                string(&element, "AXDescription").map_err(|_| "cannot read button label")?
            } else {
                title
            };
            let enabled = attribute(&element, "AXEnabled")
                .map_err(|_| "cannot read button state")?
                .downcast::<CFBoolean>()
                .map_err(|_| "invalid button state")?;
            if enabled.value() {
                buttons.push((element.clone(), label));
            }
        } else if matches!(role.as_str(), "AXStaticText" | "AXHeading") {
            let value = string(&element, "AXValue").map_err(|_| "cannot read dialog text")?;
            let value = if value.trim().is_empty() {
                string(&element, "AXTitle").map_err(|_| "cannot read dialog text")?
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
            return Err("window text exceeds inspection limit");
        }
        match elements(&element, "AXChildren") {
            Ok(children) => {
                stack.extend(children.into_iter().rev().map(|child| (child, depth + 1)))
            }
            Err(AXError::AttributeUnsupported | AXError::NoValue) => {}
            Err(_) => return Err("cannot read complete window tree"),
        }
    }
    let matches = rules.matching(
        &identity.0,
        &identity.1,
        &title,
        heading.as_deref().unwrap_or_default(),
    );
    let [(rule, selectors)] = matches.as_slice() else {
        return Err(if matches.is_empty() {
            "no enabled rule matches this window"
        } else {
            "multiple rules match; make selectors more specific"
        });
    };
    buttons.retain(|(_, label)| {
        if *rule == DIRECTORY_RULE {
            is_allow_button(label)
        } else {
            selectors.button.as_deref() == Some(label.as_str())
        }
    });
    if buttons.len() != 1 {
        return Err("expected exactly one enabled matching button");
    }
    let (button, label) = buttons.pop().unwrap();
    Ok(MatchedButton {
        button,
        signature: format!("{title}\n{text}\n{label}"),
        rule: (*rule).into(),
        label,
    })
}

fn app_name(app: &NSRunningApplication) -> String {
    app.bundleIdentifier()
        .map(|s| s.to_string())
        .or_else(|| app.localizedName().map(|s| s.to_string()))
        .unwrap_or_else(|| format!("pid {}", app.processIdentifier()))
}

fn app_identity(app: &NSRunningApplication) -> Option<(String, String)> {
    executable_path(app.processIdentifier()).map(|path| {
        (
            path,
            app.bundleIdentifier()
                .map(|s| s.to_string())
                .unwrap_or_default(),
        )
    })
}

// Match the kernel-reported executable, never an application display name.
fn executable_path(pid: i32) -> Option<String> {
    unsafe extern "C" {
        fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
    }
    let mut path = [0_i8; 4096];
    let length = unsafe { proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if length <= 0 {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(path.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
static KVO_DELIVERIES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::NSObjectNSKeyValueObserverNotification;

    define_class!(
        #[unsafe(super(NSObject))]
        struct ApplicationListFixture;
        impl ApplicationListFixture {
            #[unsafe(method(runningApplications))]
            fn applications(&self) -> usize { 0 }
        }
        unsafe impl NSObjectProtocol for ApplicationListFixture {}
    );

    #[test]
    fn lifecycle_events_reset_only_relevant_failed_processes() {
        let mut failed = HashMap::from([(100, 4), (200, 4)]);
        reset_failed(&mut failed, None, false);
        assert_eq!(failed.len(), 2);
        reset_failed(&mut failed, Some(100), false);
        assert!(!failed.contains_key(&100));
        assert_eq!(failed.get(&200), Some(&4));
        reset_failed(&mut failed, None, true);
        assert!(failed.is_empty());
    }

    #[test]
    fn release_regression_transient_subscription_errors_reach_retry_scheduler() {
        assert_eq!(
            process_subscriptions(|_| AXError::CannotComplete).unwrap_err(),
            AXError::CannotComplete
        );
        assert_eq!(
            process_subscriptions(|_| AXError::InvalidUIElement).unwrap_err(),
            AXError::InvalidUIElement
        );
        assert_eq!(
            process_subscriptions(|_| AXError::NotificationUnsupported).unwrap_err(),
            AXError::NotificationUnsupported
        );
        // A transient window-created failure cannot be hidden by a focus-only success.
        assert_eq!(
            process_subscriptions(|name| if name == "AXWindowCreated" {
                AXError::CannotComplete
            } else {
                AXError::Success
            })
            .unwrap_err(),
            AXError::CannotComplete
        );
        assert_eq!(
            process_subscriptions(|name| if name == "AXSheetCreated" {
                AXError::NotificationUnsupported
            } else {
                AXError::Success
            })
            .unwrap()
            .len(),
            2
        );
    }

    #[test]
    fn kvo_application_list_subscription_delivers_and_unregisters() {
        // An owned NSObject fixture, never the real desktop or a system UI host.
        let fixture: Retained<ApplicationListFixture> =
            unsafe { msg_send![ApplicationListFixture::alloc(), init] };
        let observer: Retained<ApplicationsObserver> =
            unsafe { msg_send![ApplicationsObserver::alloc(), init] };
        let key = ns_string!("runningApplications");
        unsafe {
            fixture.addObserver_forKeyPath_options_context(
                &observer,
                key,
                NSKeyValueObservingOptions::empty(),
                std::ptr::null_mut(),
            );
        }
        let before = KVO_DELIVERIES.load(Ordering::SeqCst);
        fixture.willChangeValueForKey(key);
        fixture.didChangeValueForKey(key);
        assert_eq!(KVO_DELIVERIES.load(Ordering::SeqCst), before + 1);
        unsafe {
            fixture.removeObserver_forKeyPath(&observer, key);
        }
        fixture.willChangeValueForKey(key);
        fixture.didChangeValueForKey(key);
        assert_eq!(KVO_DELIVERIES.load(Ordering::SeqCst), before + 1);
    }
}
