use crate::{Win32Error, WM_QUIT_LLHOOK_THREAD};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, PeekMessageW, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, MSG, PM_NOREMOVE, WH_MOUSE_LL, WM_LBUTTONDOWN,
    WM_MBUTTONDOWN, WM_RBUTTONDOWN, WM_XBUTTONDOWN,
};

static USER_PRESS_COUNT: AtomicU64 = AtomicU64::new(0);
static KEYBOARD_OBSERVER_INSTALLED: AtomicBool = AtomicBool::new(false);
static MOUSE_OBSERVER_INSTALLED: AtomicBool = AtomicBool::new(false);

pub fn user_press_count() -> Option<u64> {
    let count = USER_PRESS_COUNT.load(Ordering::SeqCst);
    if !KEYBOARD_OBSERVER_INSTALLED.load(Ordering::SeqCst)
        || !MOUSE_OBSERVER_INSTALLED.load(Ordering::SeqCst)
    {
        return None;
    }
    // Reject a snapshot spanning an observer gap, even if both hooks are back by now.
    (USER_PRESS_COUNT.load(Ordering::SeqCst) == count).then_some(count)
}

pub(crate) fn set_keyboard_observer_installed(installed: bool) {
    set_observer_installed(&KEYBOARD_OBSERVER_INSTALLED, installed);
}

pub(crate) fn set_mouse_observer_installed(installed: bool) {
    set_observer_installed(&MOUSE_OBSERVER_INSTALLED, installed);
}

fn set_observer_installed(observer: &AtomicBool, installed: bool) {
    if installed {
        record_user_press();
        observer.store(true, Ordering::SeqCst);
    } else {
        observer.store(false, Ordering::SeqCst);
        record_user_press();
    }
}

pub(crate) fn record_user_press() {
    USER_PRESS_COUNT.fetch_add(1, Ordering::SeqCst);
}

pub struct MousePressHookHandle {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MousePressHookHandle {
    fn drop(&mut self) {
        set_mouse_observer_installed(false);
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT_LLHOOK_THREAD, WPARAM(0), LPARAM(0));
        }
        if let Some(thread) = self.thread.take() {
            for _ in 0..30 {
                if thread.is_finished() {
                    let _ = thread.join();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

pub fn install_mouse_press_hook() -> Result<MousePressHookHandle, Win32Error> {
    let (init_tx, init_rx) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("mouse-press-hook".into())
        .spawn(move || unsafe {
            let thread_id = GetCurrentThreadId();
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
            let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_press_hook_proc), None, 0) {
                Ok(hook) => hook,
                Err(error) => {
                    let _ = init_tx.send(Err(Win32Error::HookInstallFailed(format!(
                        "SetWindowsHookExW for mouse presses failed: {error}"
                    ))));
                    return;
                }
            };
            set_mouse_observer_installed(true);
            let _ = init_tx.send(Ok(thread_id));
            loop {
                let ret = GetMessageW(&mut msg, None, 0, 0).0;
                if ret <= 0 || msg.message == WM_QUIT_LLHOOK_THREAD {
                    break;
                }
                let _ = DispatchMessageW(&msg);
            }
            set_mouse_observer_installed(false);
            let _ = UnhookWindowsHookEx(hook);
        })
        .map_err(|error| {
            Win32Error::HookInstallFailed(format!(
                "Failed to spawn mouse press hook thread: {error}"
            ))
        })?;
    let thread_id = init_rx.recv().map_err(|_| {
        Win32Error::HookInstallFailed("Mouse press hook thread initialization failed".into())
    })??;
    Ok(MousePressHookHandle {
        thread_id,
        thread: Some(thread),
    })
}

unsafe extern "system" fn mouse_press_hook_proc(
    ncode: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if ncode >= 0
        && matches!(
            wparam.0 as u32,
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
        )
    {
        record_user_press();
    }
    CallNextHookEx(None, ncode, wparam, lparam)
}

#[cfg(test)]
pub(crate) static PRESS_COUNT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_LBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    };

    #[test]
    fn press_count_requires_both_observers_and_invalidates_observation_gaps() {
        let _guard = PRESS_COUNT_TEST_LOCK.lock().unwrap();
        set_keyboard_observer_installed(false);
        set_mouse_observer_installed(false);
        assert_eq!(user_press_count(), None);
        for (keyboard, installed, available) in [
            (true, true, false),
            (false, true, true),
            (true, false, false),
            (true, true, true),
            (false, false, false),
            (false, true, true),
            (true, false, false),
            (false, false, false),
        ] {
            let before = USER_PRESS_COUNT.load(Ordering::SeqCst);
            if keyboard {
                set_keyboard_observer_installed(installed);
            } else {
                set_mouse_observer_installed(installed);
            }
            let after = USER_PRESS_COUNT.load(Ordering::SeqCst);
            assert_eq!(after, before + 1);
            assert_eq!(user_press_count(), available.then_some(after));
        }
    }

    #[test]
    fn mouse_hook_counts_button_downs_but_not_motion_wheel_or_release() {
        let _guard = PRESS_COUNT_TEST_LOCK.lock().unwrap();
        set_keyboard_observer_installed(true);
        set_mouse_observer_installed(true);
        let mut expected = user_press_count().unwrap();
        for (ncode, message, counts) in [
            (0, WM_MOUSEMOVE, false),
            (0, WM_MOUSEWHEEL, false),
            (0, WM_MOUSEHWHEEL, false),
            (0, WM_LBUTTONUP, false),
            (-1, WM_LBUTTONDOWN, false),
            (0, WM_LBUTTONDOWN, true),
            (0, WM_RBUTTONDOWN, true),
            (0, WM_MBUTTONDOWN, true),
            (0, WM_XBUTTONDOWN, true),
        ] {
            unsafe { mouse_press_hook_proc(ncode, WPARAM(message as usize), LPARAM(0)) };
            expected += u64::from(counts);
            assert_eq!(
                user_press_count(),
                Some(expected),
                "message={message}, ncode={ncode}"
            );
        }
        set_keyboard_observer_installed(false);
        set_mouse_observer_installed(false);
    }
}
