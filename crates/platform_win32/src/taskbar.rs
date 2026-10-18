//! Hide/show taskbar buttons for managed windows via `ITaskbarList`.
//!
//! DWM cloaking can't hide an external window's taskbar button (it returns
//! `E_ACCESSDENIED` on another process's window) and off-screen positioning
//! doesn't reliably drop the button either, so `ITaskbarList::DeleteTab` /
//! `AddTab` is the only mechanism that works. The interface is apartment-model
//! COM, so a dedicated STA thread owns it and processes requests over a channel.
//!
//! Callable from anywhere via [`taskbar_hide`] / [`taskbar_show`]. Hidden buttons
//! are tracked for best-effort restoration when [`TaskbarHandle`] is dropped.
//! Shutdown detaches the worker if shell calls outlast the bounded wait.

use crate::recover_poisoned_mutex;
use leopardwm_core_layout::WindowId;
use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{ITaskbarList, TaskbarList};

enum TaskbarCmd {
    Hide(WindowId),
    Show(WindowId),
    Restore(WindowId),
    Forget(WindowId),
}

/// Global taskbar connection. Its sender is cleared by ordinary or emergency shutdown.
/// The completion flag remains available to repeated emergency disconnects.
struct TaskbarSender {
    tx: Option<mpsc::Sender<TaskbarCmd>>,
    stopped: Arc<AtomicBool>,
}

static TASKBAR_TX: Mutex<Option<TaskbarSender>> = Mutex::new(None);

pub(crate) fn emergency_disconnect() -> Option<Arc<AtomicBool>> {
    let mut sender = TASKBAR_TX.try_lock().ok()?;
    let sender = sender.as_mut()?;
    sender.tx.take();
    Some(sender.stopped.clone())
}

/// Remove `wid`'s taskbar button (best-effort; no-op if uninitialized).
pub fn taskbar_hide(wid: WindowId) {
    let guard = TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex);
    if let Some(tx) = guard.as_ref().and_then(|sender| sender.tx.as_ref()) {
        let _ = tx.send(TaskbarCmd::Hide(wid));
    }
}

/// Restore `wid`'s taskbar button (only acts if we'd hidden it).
pub fn taskbar_show(wid: WindowId) {
    let guard = TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex);
    if let Some(tx) = guard.as_ref().and_then(|sender| sender.tx.as_ref()) {
        let _ = tx.send(TaskbarCmd::Show(wid));
    }
}

/// Unconditionally re-add `wid`'s taskbar button. Used at startup to restore
/// buttons a crashed prior daemon instance may have left deleted (those aren't
/// in this process's hidden set, so `taskbar_show` wouldn't touch them).
pub fn taskbar_restore(wid: WindowId) {
    let guard = TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex);
    if let Some(tx) = guard.as_ref().and_then(|sender| sender.tx.as_ref()) {
        let _ = tx.send(TaskbarCmd::Restore(wid));
    }
}

/// Drop `wid` from the hidden set without an `AddTab` (the window is gone).
/// Keeps the set from retaining a stale id whose HWND could be recycled, which
/// would otherwise make the change-gate skip re-hiding the new window.
pub fn taskbar_forget(wid: WindowId) {
    let guard = TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex);
    if let Some(tx) = guard.as_ref().and_then(|sender| sender.tx.as_ref()) {
        let _ = tx.send(TaskbarCmd::Forget(wid));
    }
}

/// Owns the taskbar-control thread. Dropping it requests restoration of hidden
/// taskbar buttons and waits up to roughly 500 ms before detaching the worker.
pub struct TaskbarHandle {
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Spawn the COM thread and publish the global sender. Returns `None` if the
/// thread can't be spawned; taskbar hiding is then a best-effort no-op.
pub fn init_taskbar() -> Option<TaskbarHandle> {
    let (tx, rx) = mpsc::channel::<TaskbarCmd>();
    let stopped = Arc::new(AtomicBool::new(false));
    let worker_stopped = stopped.clone();
    let thread = std::thread::Builder::new()
        .name("taskbar-list".into())
        .spawn(move || {
            run(rx);
            worker_stopped.store(true, Ordering::Release);
        })
        .map_err(|e| tracing::warn!("Failed to spawn taskbar thread: {}", e))
        .ok()?;
    *TASKBAR_TX.lock().unwrap_or_else(recover_poisoned_mutex) = Some(TaskbarSender {
        tx: Some(tx),
        stopped,
    });
    Some(TaskbarHandle {
        thread: Some(thread),
    })
}

impl Drop for TaskbarHandle {
    fn drop(&mut self) {
        // Closing the channel lets the worker restore hidden buttons. Bound the
        // wait so a hung shell call cannot block shutdown forever.
        if let Some(sender) = TASKBAR_TX
            .lock()
            .unwrap_or_else(recover_poisoned_mutex)
            .as_mut()
        {
            sender.tx.take();
        }
        if let Some(thread) = self.thread.take() {
            for _ in 0..50 {
                if thread.is_finished() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                tracing::warn!("Taskbar thread did not exit promptly; detaching");
            }
        }
    }
}

fn hwnd_of(wid: WindowId) -> HWND {
    HWND(wid as *mut c_void)
}

fn run(rx: mpsc::Receiver<TaskbarCmd>) {
    unsafe {
        // STA: ITaskbarList is apartment-threaded.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let taskbar: ITaskbarList = match CoCreateInstance(&TaskbarList, None, CLSCTX_ALL) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("CoCreateInstance(TaskbarList) failed: {}", e);
                CoUninitialize();
                return;
            }
        };
        if let Err(e) = taskbar.HrInit() {
            tracing::warn!("ITaskbarList::HrInit failed: {}", e);
            CoUninitialize();
            return;
        }
        tracing::info!("Taskbar controller initialized");

        let mut hidden: HashSet<WindowId> = HashSet::new();
        while let Ok(cmd) = rx.recv() {
            match cmd {
                // Change-gated so frequent focus/scroll reconciles don't spam
                // the shell with redundant DeleteTab/AddTab calls.
                TaskbarCmd::Hide(wid) => {
                    if hidden.insert(wid) {
                        let _ = taskbar.DeleteTab(hwnd_of(wid));
                    }
                }
                TaskbarCmd::Show(wid) => {
                    if hidden.remove(&wid) {
                        let _ = taskbar.AddTab(hwnd_of(wid));
                    }
                }
                TaskbarCmd::Restore(wid) => {
                    hidden.remove(&wid);
                    let _ = taskbar.AddTab(hwnd_of(wid));
                }
                TaskbarCmd::Forget(wid) => {
                    hidden.remove(&wid);
                }
            }
        }

        // Channel closure requests best-effort restoration, including on emergency disconnect.
        for wid in hidden.drain() {
            let _ = taskbar.AddTab(hwnd_of(wid));
        }
        drop(taskbar);
        CoUninitialize();
        tracing::debug!("Taskbar controller stopped (restored all tabs)");
    }
}
