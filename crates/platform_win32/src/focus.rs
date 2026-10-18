//! Foreground/focus management and graceful window close.

use crate::types::Win32Error;
use crate::window_id_to_hwnd;
use leopardwm_core_layout::WindowId;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GetForegroundWindow, GetTopWindow, GetWindow, GetWindowLongW, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindow, PostMessageW, SetCursorPos, SetForegroundWindow,
    SetWindowPos, ShowWindow, GWL_EXSTYLE, GW_HWNDNEXT, HWND_TOP, SET_WINDOW_POS_FLAGS,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_RESTORE, SW_SHOWNOACTIVATE, WS_EX_TOPMOST,
};

/// The current OS foreground window as a `WindowId`, if any. This is
/// authoritative at the moment of the call, unlike the daemon's cached
/// focus, so callers that must know the truly-focused window at a precise
/// instant (e.g. recording which window was focused on a workspace before
/// leaving it) can query it directly.
pub fn get_foreground_window() -> Option<WindowId> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.0.is_null()).then_some(hwnd.0 as WindowId)
}

/// Current time in the same wrapping millisecond domain as WinEvent timestamps.
pub fn current_event_time_ms() -> u32 {
    use windows::Win32::System::SystemInformation::GetTickCount;
    unsafe { GetTickCount() }
}

/// Milliseconds since the last input event reported by Windows (keyboard or mouse).
/// Used as a heuristic for whether a focus change was user-initiated; this does
/// not distinguish physical from injected input. Returns `None` if the API fails.
pub fn ms_since_last_user_input() -> Option<u32> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    unsafe {
        let mut lii = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        if !GetLastInputInfo(&mut lii).as_bool() {
            return None;
        }
        Some(GetTickCount().wrapping_sub(lii.dwTime))
    }
}

/// Move the mouse cursor to the center of `hwnd` (the "mouse follows focus"
/// behavior). Best-effort: does nothing if the handle is invalid or its rect
/// can't be read.
pub fn warp_cursor_to_window(hwnd: WindowId) {
    let Ok(hwnd) = window_id_to_hwnd(hwnd) else {
        return;
    };
    unsafe {
        // A minimized window reports off-screen (-32000, -32000) coordinates,
        // so warping to its rect would fling the cursor into the void.
        if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() {
            return;
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return;
        }
        let cx = rect.left + (rect.right - rect.left) / 2;
        let cy = rect.top + (rect.bottom - rect.top) / 2;
        let _ = SetCursorPos(cx, cy);
    }
}

/// Restore a minimized window without activating it.
///
/// The operation is idempotent: visible windows are left untouched. `ShowWindow`
/// reports the previous visibility state rather than restore success, so success
/// is determined by checking whether the window remains minimized afterward.
pub fn restore_window_no_activate(window_id: WindowId) -> Result<(), Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        restore_window_no_activate_with(
            window_id,
            || IsWindow(Some(hwnd)).as_bool(),
            || IsIconic(hwnd).as_bool(),
            || {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            },
        )
    }
}

/// Upper bound on how long the style worker waits for an owner-thread
/// (`ShowWindowAsync`) restore to take effect before reporting the window as
/// still maximized. Short enough to keep admission snappy, long enough to
/// cover an owner that only pumps between frames.
pub(crate) const MAXIMIZED_ASYNC_RESTORE_WAIT_MS: u32 = 500;
const MAXIMIZED_ASYNC_RESTORE_POLL_MS: u32 = 10;

/// Restore a maximized window without activating it.
///
/// A cross-process `ShowWindow(SW_SHOWNOACTIVATE)` can deliver the window
/// position messages yet leave the window zoomed, so it may report success
/// while nothing changed. When that happens the restore is reposted to the
/// owner's thread with `ShowWindowAsync`, which runs the show in the window's
/// own context and so cannot be ignored the same way, nor block this worker on
/// a busy owner. The outcome is then polled for a bounded time.
///
/// `target_ok` reports that the admitted window is still live,
/// identity-matching and visible, independent of its zoom state. It is read
/// before every zoom result, because a destroyed or replaced handle makes
/// `IsZoomed` report false, which would otherwise look like a successful
/// restore.
pub(crate) fn restore_maximized_window_no_activate_with(
    window_id: WindowId,
    target_ok: impl Fn() -> bool,
    is_zoomed: impl Fn() -> bool,
    show_window: impl FnOnce(),
    show_window_async: impl FnOnce(),
    mut sleep: impl FnMut(u32),
) -> Result<(), Win32Error> {
    if !target_ok() {
        return Err(Win32Error::WindowNotFound(window_id));
    }
    if !is_zoomed() {
        return Ok(());
    }
    show_window();
    if !target_ok() {
        return Err(Win32Error::WindowNotFound(window_id));
    }
    if !is_zoomed() {
        return Ok(());
    }
    show_window_async();
    let mut waited_ms = 0;
    loop {
        if !target_ok() {
            return Err(Win32Error::WindowNotFound(window_id));
        }
        if !is_zoomed() {
            tracing::debug!(
                "Async fallback restored maximized window {} after {} ms",
                window_id,
                waited_ms
            );
            return Ok(());
        }
        if waited_ms >= MAXIMIZED_ASYNC_RESTORE_WAIT_MS {
            break;
        }
        sleep(MAXIMIZED_ASYNC_RESTORE_POLL_MS);
        waited_ms += MAXIMIZED_ASYNC_RESTORE_POLL_MS;
    }
    tracing::debug!(
        "Async fallback did not restore maximized window {} within {} ms",
        window_id,
        MAXIMIZED_ASYNC_RESTORE_WAIT_MS
    );
    Err(Win32Error::SetPositionFailed(format!(
        "Failed to restore maximized window {} without activation",
        window_id
    )))
}

fn restore_window_no_activate_with(
    window_id: WindowId,
    is_window: impl Fn() -> bool,
    is_iconic: impl Fn() -> bool,
    show_window: impl FnOnce(),
) -> Result<(), Win32Error> {
    if !is_window() {
        return Err(Win32Error::WindowNotFound(window_id));
    }
    if !is_iconic() {
        return Ok(());
    }
    show_window();
    if !is_window() {
        return Err(Win32Error::WindowNotFound(window_id));
    }
    if is_iconic() {
        return Err(Win32Error::SetPositionFailed(format!(
            "Failed to restore minimized window {} without activation",
            window_id
        )));
    }
    Ok(())
}

/// Set the foreground window using Win32 SetForegroundWindow.
///
/// Uses AttachThreadInput trick to reliably set foreground even when
/// the calling process is not the foreground process.
pub fn set_foreground_window(hwnd: WindowId) -> Result<bool, Win32Error> {
    let window_id = hwnd;
    let hwnd = window_id_to_hwnd(window_id)?;

    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(Win32Error::WindowNotFound(window_id));
        }

        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            if IsIconic(hwnd).as_bool() {
                return Err(Win32Error::SetPositionFailed(format!(
                    "Failed to restore minimized window {} before setting foreground",
                    window_id
                )));
            }
        }

        let target_thread = GetWindowThreadProcessId(hwnd, None);
        if target_thread == 0 {
            return Err(Win32Error::SetPositionFailed(format!(
                "GetWindowThreadProcessId returned 0 for window {}",
                window_id
            )));
        }
        let current_thread = GetCurrentThreadId();
        let mut diagnostics: Vec<String> = Vec::new();

        // Attach our input queue to BOTH the foreground and target threads so
        // Windows permits the foreground change, including from an active holder
        // such as a borderless fullscreen game. Low-level keyboard hook delivery
        // does not grant the "last input event" foreground right that RegisterHotKey
        // provides; without the foreground attachment the window can scroll into
        // place but remain behind the foreground app.
        let foreground_thread = {
            let fg = GetForegroundWindow();
            if fg.0.is_null() {
                0
            } else {
                GetWindowThreadProcessId(fg, None)
            }
        };
        let mut attached: Vec<u32> = Vec::new();
        for candidate in [foreground_thread, target_thread] {
            if candidate != 0 && candidate != current_thread && !attached.contains(&candidate) {
                if windows::Win32::System::Threading::AttachThreadInput(
                    current_thread,
                    candidate,
                    true,
                )
                .as_bool()
                {
                    attached.push(candidate);
                } else {
                    diagnostics.push(format!(
                        "AttachThreadInput attach failed (current_thread={}, other_thread={})",
                        current_thread, candidate
                    ));
                }
            }
        }

        let mut foreground_set = SetForegroundWindow(hwnd).as_bool();

        if !foreground_set {
            match BringWindowToTop(hwnd) {
                Ok(()) => {
                    foreground_set = SetForegroundWindow(hwnd).as_bool();
                    if !foreground_set {
                        diagnostics.push(
                            "SetForegroundWindow returned FALSE after BringWindowToTop fallback"
                                .to_string(),
                        );
                    }
                }
                Err(e) => diagnostics.push(format!("BringWindowToTop failed: {}", e)),
            }
        }

        // Detach every input queue we attached to.
        for thread in &attached {
            if !windows::Win32::System::Threading::AttachThreadInput(current_thread, *thread, false)
                .as_bool()
            {
                diagnostics.push(format!(
                    "AttachThreadInput detach failed (current_thread={}, other_thread={})",
                    current_thread, thread
                ));
            }
        }

        if foreground_set {
            if !diagnostics.is_empty() {
                tracing::warn!(
                    "Foreground set for window {} with warnings: {}",
                    window_id,
                    diagnostics.join("; ")
                );
            }
            return Ok(true);
        }

        if diagnostics.is_empty() {
            // No explicit API error, but Windows denied foreground change.
            return Ok(false);
        }

        Err(Win32Error::SetPositionFailed(format!(
            "Failed to set foreground window {}: {}",
            window_id,
            diagnostics.join("; ")
        )))
    }
}

// Raises reject every cloak reason; enumeration retains its shell-only admission mask.
fn dwm_cloak_blocks_raise(cloaked: u32) -> bool {
    cloaked != 0
}

fn is_window_dwm_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_err()
            || dwm_cloak_blocks_raise(cloaked)
    }
}

/// Visible, non-minimized top-level windows, ordered from top to bottom.
pub fn visible_window_z_order() -> Vec<WindowId> {
    let mut windows = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut next = unsafe { GetTopWindow(None).ok() };
    while let Some(hwnd) = next.filter(|hwnd| !hwnd.is_invalid()) {
        let id = hwnd.0 as WindowId;
        if !seen.insert(id) {
            break;
        }
        if crate::is_window_visible(id)
            && !unsafe { IsIconic(hwnd).as_bool() }
            && !is_window_dwm_cloaked(hwnd)
            && !crate::is_placement_cloaked(id)
            && !crate::is_placement_parked(id)
        {
            windows.push(id);
        }
        next = unsafe { GetWindow(hwnd, GW_HWNDNEXT).ok() };
    }
    windows
}

fn normal_window_can_raise(visible: bool, minimized: bool, cloaked: bool, ex_style: u32) -> bool {
    visible && !minimized && !cloaked && ex_style & WS_EX_TOPMOST.0 == 0
}

pub fn can_raise_normal_window(window_id: WindowId) -> bool {
    let Ok(hwnd) = window_id_to_hwnd(window_id) else {
        return false;
    };
    unsafe {
        normal_window_can_raise(
            IsWindow(Some(hwnd)).as_bool() && crate::is_window_visible(window_id),
            IsIconic(hwnd).as_bool(),
            is_window_dwm_cloaked(hwnd)
                || crate::is_placement_cloaked(window_id)
                || crate::is_placement_parked(window_id),
            GetWindowLongW(hwnd, GWL_EXSTYLE) as u32,
        )
    }
}

pub fn raise_normal_window_no_activate(window_id: WindowId) -> Result<(), Win32Error> {
    if !can_raise_normal_window(window_id) {
        return Ok(());
    }
    raise_window_no_activate(window_id)
}

const RAISE_WINDOW_FLAGS: SET_WINDOW_POS_FLAGS =
    SET_WINDOW_POS_FLAGS(SWP_NOMOVE.0 | SWP_NOSIZE.0 | SWP_NOACTIVATE.0);

/// Raise a window within its existing z-order band without activating it.
pub fn raise_window_no_activate(window_id: WindowId) -> Result<(), Win32Error> {
    let hwnd = window_id_to_hwnd(window_id)?;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return Err(Win32Error::WindowNotFound(window_id));
        }
        SetWindowPos(hwnd, Some(HWND_TOP), 0, 0, 0, 0, RAISE_WINDOW_FLAGS).map_err(|e| {
            Win32Error::SetPositionFailed(format!(
                "Failed to raise window {} without activation: {}",
                window_id, e
            ))
        })?;
    }
    Ok(())
}

/// Close a window by posting WM_CLOSE.
///
/// This is a graceful close that allows the application to handle cleanup.
pub fn close_window(hwnd: WindowId) -> Result<(), Win32Error> {
    let hwnd = window_id_to_hwnd(hwnd)?;
    unsafe {
        const WM_CLOSE: u32 = 0x0010;
        PostMessageW(
            Some(hwnd),
            WM_CLOSE,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        )
        .map_err(|e| {
            Win32Error::SetPositionFailed(format!("PostMessageW(WM_CLOSE) failed: {}", e))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn floating_above_tiled_rejects_any_dwm_cloak() {
        for (cloak, blocked) in [
            (0, false),
            (0x1, true),
            (0x2, true),
            (0x4, true),
            (0x7, true),
        ] {
            assert_eq!(dwm_cloak_blocks_raise(cloak), blocked, "cloak {cloak:#x}");
        }
    }

    #[test]
    fn floating_above_tiled_normal_band_eligibility() {
        for (visible, minimized, cloaked, style, expected) in [
            (true, false, false, 0, true),
            (false, false, false, 0, false),
            (true, true, false, 0, false),
            (true, false, true, 0, false),
            (true, false, false, WS_EX_TOPMOST.0, false),
            (true, false, false, WS_EX_TOPMOST.0 | 0x80, false),
        ] {
            assert_eq!(
                normal_window_can_raise(visible, minimized, cloaked, style),
                expected
            );
        }
    }

    #[test]
    fn floating_above_tiled_raise_flags_preserve_focus_and_geometry() {
        assert_eq!(
            RAISE_WINDOW_FLAGS.0,
            SWP_NOACTIVATE.0 | SWP_NOMOVE.0 | SWP_NOSIZE.0
        );
    }
    use super::*;

    /// Stand-in for one admitted target window: alive, identity-matching,
    /// visible and zoomed until the scripted action changes it. `generation`
    /// stands in for the identity and managed-lifetime token, which a handle
    /// recycled by a replacement window fails even while it is live and zoomed.
    #[derive(Default)]
    struct Target {
        alive: std::cell::Cell<bool>,
        visible: std::cell::Cell<bool>,
        zoomed: std::cell::Cell<bool>,
        generation: std::cell::Cell<u32>,
        async_posts: std::cell::Cell<u32>,
        slept_ms: std::cell::Cell<u32>,
    }

    impl Target {
        fn new() -> Self {
            Self {
                alive: std::cell::Cell::new(true),
                visible: std::cell::Cell::new(true),
                zoomed: std::cell::Cell::new(true),
                ..Default::default()
            }
        }

        fn is_zoomed(&self) -> bool {
            self.alive.get() && self.zoomed.get()
        }

        fn destroy(&self) {
            self.alive.set(false);
            self.zoomed.set(false);
        }

        /// The original is destroyed and a new window recycles the handle.
        fn recycle(&self) {
            self.alive.set(true);
            self.generation.set(self.generation.get() + 1);
        }

        fn hide(&self) {
            self.visible.set(false);
        }

        fn restore(&self) {
            self.zoomed.set(false);
        }

        fn run(&self, on_sync: impl FnOnce(), on_poll: impl Fn(u32)) -> Result<(), Win32Error> {
            let admitted_generation = self.generation.get();
            let is_target = || {
                self.alive.get()
                    && self.visible.get()
                    && self.generation.get() == admitted_generation
            };
            let mut polls = 0u32;
            restore_maximized_window_no_activate_with(
                42,
                is_target,
                || self.is_zoomed(),
                on_sync,
                || self.async_posts.set(self.async_posts.get() + 1),
                |ms| {
                    self.slept_ms.set(self.slept_ms.get() + ms);
                    on_poll(polls);
                    polls += 1;
                },
            )
        }
    }

    #[test]
    fn restore_maximized_without_activation_verifies_restored_state() {
        let target = Target::new();
        target.run(|| target.restore(), |_| {}).unwrap();

        assert!(!target.zoomed.get());
        assert_eq!(target.async_posts.get(), 0, "sync restore succeeded");
    }

    #[test]
    fn restore_maximized_without_activation_skips_normal_window() {
        let target = Target::new();
        target.zoomed.set(false);
        target
            .run(|| panic!("must not restore a normal window"), |_| {})
            .unwrap();

        assert_eq!(target.async_posts.get(), 0);
    }

    #[test]
    fn restore_maximized_without_activation_falls_back_to_owner_thread() {
        let target = Target::new();
        // The async post only lands on the second poll, mirroring an owner
        // thread that reaches its message queue after a short delay.
        target
            .run(
                || {},
                |poll| {
                    if poll == 1 {
                        target.restore();
                    }
                },
            )
            .unwrap();

        assert_eq!(target.async_posts.get(), 1, "fallback runs exactly once");
        assert_eq!(target.slept_ms.get(), 2 * MAXIMIZED_ASYNC_RESTORE_POLL_MS);
    }

    #[test]
    fn restore_maximized_without_activation_reports_window_that_ignores_fallback() {
        let target = Target::new();
        let result = target.run(|| {}, |_| {});

        assert!(matches!(result, Err(Win32Error::SetPositionFailed(_))));
        assert_eq!(
            target.slept_ms.get(),
            MAXIMIZED_ASYNC_RESTORE_WAIT_MS,
            "wait is bounded"
        );
    }

    #[test]
    fn restore_maximized_without_activation_skips_fallback_for_hidden_window() {
        let target = Target::new();
        // The owner hides the window while handling the synchronous attempt.
        let result = target.run(
            || target.hide(),
            |_| panic!("must not poll a hidden window"),
        );

        assert!(matches!(result, Err(Win32Error::WindowNotFound(42))));
        assert_eq!(
            target.async_posts.get(),
            0,
            "a hidden window must not be re-shown"
        );
    }

    #[test]
    fn restore_maximized_without_activation_skips_fallback_for_replacement_window() {
        let target = Target::new();
        // The synchronous attempt destroys the window and a still zoomed
        // replacement recycles the handle.
        let result = target.run(
            || target.recycle(),
            |_| panic!("must not poll a replacement window"),
        );

        assert!(matches!(result, Err(Win32Error::WindowNotFound(42))));
        assert_eq!(
            target.async_posts.get(),
            0,
            "the fallback must not restore a replacement"
        );
    }

    #[test]
    fn restore_maximized_without_activation_reports_destroyed_window_during_wait() {
        let target = Target::new();
        let result = target.run(
            || {},
            |poll| {
                if poll == 0 {
                    target.destroy();
                }
            },
        );

        assert!(matches!(result, Err(Win32Error::WindowNotFound(42))));
        assert_eq!(
            target.slept_ms.get(),
            MAXIMIZED_ASYNC_RESTORE_POLL_MS,
            "stops on the first poll that sees the window gone"
        );
    }

    #[test]
    fn restore_maximized_without_activation_sees_a_restore_at_the_last_poll() {
        let target = Target::new();
        let last_poll = MAXIMIZED_ASYNC_RESTORE_WAIT_MS / MAXIMIZED_ASYNC_RESTORE_POLL_MS;
        let result = target.run(
            || {},
            |poll| {
                if poll == last_poll - 1 {
                    target.restore();
                }
            },
        );

        result.unwrap();
        assert_eq!(target.slept_ms.get(), MAXIMIZED_ASYNC_RESTORE_WAIT_MS);
    }

    #[test]
    fn restore_maximized_without_activation_sees_destruction_at_the_last_poll() {
        let target = Target::new();
        let last_poll = MAXIMIZED_ASYNC_RESTORE_WAIT_MS / MAXIMIZED_ASYNC_RESTORE_POLL_MS;
        let result = target.run(
            || {},
            |poll| {
                if poll == last_poll - 1 {
                    target.destroy();
                }
            },
        );

        assert!(
            matches!(result, Err(Win32Error::WindowNotFound(42))),
            "destruction in the final interval is not a timeout"
        );
        assert_eq!(target.slept_ms.get(), MAXIMIZED_ASYNC_RESTORE_WAIT_MS);
    }

    #[test]
    fn restore_maximized_without_activation_rejects_dead_window() {
        let target = Target::new();
        target.destroy();
        let result = target.run(|| panic!("must not touch a dead window"), |_| {});

        assert!(matches!(result, Err(Win32Error::WindowNotFound(42))));
    }

    #[test]
    fn restore_without_activation_skips_visible_window() {
        let show_calls = std::cell::Cell::new(0);
        restore_window_no_activate_with(
            42,
            || true,
            || false,
            || show_calls.set(show_calls.get() + 1),
        )
        .unwrap();
        assert_eq!(show_calls.get(), 0);
    }

    #[test]
    fn restore_without_activation_verifies_restored_state() {
        let iconic = std::cell::Cell::new(true);
        restore_window_no_activate_with(42, || true, || iconic.get(), || iconic.set(false))
            .unwrap();
        assert!(!iconic.get());
    }

    #[test]
    fn restore_without_activation_rejects_dead_window() {
        let result = restore_window_no_activate_with(42, || false, || false, || {});
        assert!(matches!(result, Err(Win32Error::WindowNotFound(42))));
    }

    #[test]
    fn restore_without_activation_reports_window_that_stays_minimized() {
        let result = restore_window_no_activate_with(42, || true, || true, || {});
        assert!(matches!(result, Err(Win32Error::SetPositionFailed(_))));
    }

    #[test]
    fn restore_without_activation_rejects_window_destroyed_during_restore() {
        let alive = std::cell::Cell::new(true);
        let iconic = std::cell::Cell::new(true);
        let result = restore_window_no_activate_with(
            42,
            || alive.get(),
            || iconic.get(),
            || {
                iconic.set(false);
                alive.set(false);
            },
        );

        assert!(matches!(result, Err(Win32Error::WindowNotFound(42))));
    }

    #[test]
    fn restore_without_activation_zero_fails() {
        let result = restore_window_no_activate(0);
        assert!(matches!(result, Err(Win32Error::WindowNotFound(0))));
    }

    #[test]
    fn restore_without_activation_invalid_hwnd_fails() {
        let result = restore_window_no_activate(u64::MAX);
        assert!(matches!(result, Err(Win32Error::WindowNotFound(u64::MAX))));
    }

    #[test]
    fn test_set_foreground_window_zero_fails() {
        let result = set_foreground_window(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_set_foreground_window_invalid_hwnd_fails() {
        let result = set_foreground_window(u64::MAX);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    #[test]
    fn test_raise_window_no_activate_zero_fails() {
        let result = raise_window_no_activate(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }

    #[test]
    fn test_raise_window_no_activate_invalid_hwnd_fails() {
        let result = raise_window_no_activate(u64::MAX);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Win32Error::WindowNotFound(u64::MAX)
        ));
    }

    #[test]
    fn test_close_window_zero_fails() {
        let result = close_window(0);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Win32Error::WindowNotFound(0)));
    }
}
