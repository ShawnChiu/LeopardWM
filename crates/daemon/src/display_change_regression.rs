use super::*;
use leopardwm_core_layout::{Visibility, WindowPlacement};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetPropW,
    IsZoomed, PostThreadMessageW, SetPropW, SetWindowLongPtrW, ShowWindow, GWLP_WNDPROC, MSG,
    SWP_NOSIZE, SW_HIDE, SW_SHOWMAXIMIZED, WINDOWPOS, WM_QUIT, WM_WINDOWPOSCHANGING,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW, WS_POPUP,
};

unsafe extern "system" fn placement_owner_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == windows::Win32::UI::WindowsAndMessaging::WM_APP + 42 {
        let _ = SetPropW(
            hwnd,
            w!("LeopardWMFixtureQueueDrained"),
            Some(HANDLE(std::ptr::dangling_mut())),
        );
        return LRESULT(0);
    }
    if message == WM_WINDOWPOSCHANGING {
        let control = GetPropW(hwnd, w!("LeopardWMFixtureMoveControl")).0;
        if !control.is_null() {
            let control = &*(control as *const MoveControl);
            let delay = control.move_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                thread::sleep(Duration::from_millis(delay));
            }
            if control.block_next.swap(false, Ordering::SeqCst) {
                let released = control.released.lock().unwrap();
                control.entered.store(true, Ordering::SeqCst);
                let (_released, _timeout) = control
                    .wake
                    .wait_timeout_while(released, Duration::from_secs(5), |released| !*released)
                    .unwrap();
            }
        }
        let position = &mut *(lparam.0 as *mut WINDOWPOS);
        if !position.flags.contains(SWP_NOSIZE) {
            let min_width = GetPropW(hwnd, w!("LeopardWMFixtureMinWidth")).0 as usize as i32;
            position.cx = position.cx.max(min_width);
        }
    }
    let result = DefWindowProcW(hwnd, message, wparam, lparam);
    if message == windows::Win32::UI::WindowsAndMessaging::WM_WINDOWPOSCHANGED {
        let control = GetPropW(hwnd, w!("LeopardWMFixtureMoveControl")).0;
        if !control.is_null() {
            let control = &*(control as *const MoveControl);
            let position = &*(lparam.0 as *const WINDOWPOS);
            control.moves.lock().unwrap().push((position.x, position.y));
            if control.pause_after_move.swap(false, Ordering::SeqCst) {
                control.pause_pump.store(true, Ordering::SeqCst);
            }
        }
    }
    result
}

#[derive(Default)]
struct MoveControl {
    block_next: AtomicBool,
    move_delay_ms: AtomicU64,
    entered: AtomicBool,
    pause_pump: AtomicBool,
    pause_after_move: AtomicBool,
    pump_paused: AtomicBool,
    moves: Mutex<Vec<(i32, i32)>>,
    released: Mutex<bool>,
    wake: Condvar,
}

impl MoveControl {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
        self.pause_pump.store(false, Ordering::SeqCst);
    }
}

struct PlacementOwner {
    window_id: u64,
    thread_id: u32,
    release: mpsc::Sender<()>,
    pumping: mpsc::Receiver<()>,
    join: Option<thread::JoinHandle<()>>,
    control: Arc<MoveControl>,
}

impl PlacementOwner {
    fn spawn() -> Self {
        Self::spawn_at(Rect::new(48, 48, 160, 120))
    }

    fn spawn_at(rect: Rect) -> Self {
        Self::spawn_with(rect, 0, false)
    }

    fn spawn_with(rect: Rect, min_width: i32, parked_maximized: bool) -> Self {
        Self::spawn_with_message_mode(rect, min_width, parked_maximized, false)
    }

    fn spawn_with_message_mode(
        rect: Rect,
        min_width: i32,
        parked_maximized: bool,
        sent_messages_only: bool,
    ) -> Self {
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let (pumping_tx, pumping) = mpsc::channel();
        let control = Arc::new(MoveControl::default());
        let owner_control = Arc::clone(&control);
        let join = thread::spawn(move || unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("display-change placement fixture"),
                if parked_maximized {
                    WS_OVERLAPPEDWINDOW
                } else {
                    WS_POPUP
                },
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            SetPropW(
                hwnd,
                w!("LeopardWMFixtureMoveControl"),
                Some(HANDLE(Arc::as_ptr(&owner_control) as *mut _)),
            )
            .unwrap();
            if min_width > 0 || sent_messages_only {
                SetPropW(
                    hwnd,
                    w!("LeopardWMFixtureMinWidth"),
                    Some(HANDLE(min_width as usize as *mut _)),
                )
                .unwrap();
                SetWindowLongPtrW(
                    hwnd,
                    GWLP_WNDPROC,
                    placement_owner_proc as *const () as isize,
                );
            }
            if parked_maximized {
                let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
                if !IsZoomed(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_SHOWMAXIMIZED);
                }
                let _ = ShowWindow(hwnd, SW_HIDE);
                leopardwm_platform_win32::park_window_for_placement(hwnd.0 as usize as u64)
                    .unwrap();
                assert!(IsZoomed(hwnd).as_bool());
            }
            ready_tx
                .send((hwnd.0 as usize as u64, GetCurrentThreadId()))
                .unwrap();
            if sent_messages_only {
                let deadline = Instant::now() + Duration::from_secs(20);
                while resume.try_recv().is_err() && Instant::now() < deadline {
                    if owner_control.pause_pump.load(Ordering::SeqCst) {
                        owner_control.pump_paused.store(true, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    owner_control.pump_paused.store(false, Ordering::SeqCst);
                    let mut message = MSG::default();
                    let _ = windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                        &mut message,
                        None,
                        WM_QUIT,
                        WM_QUIT,
                        windows::Win32::UI::WindowsAndMessaging::PEEK_MESSAGE_REMOVE_TYPE(
                            windows::Win32::UI::WindowsAndMessaging::QS_SENDMESSAGE.0 << 16,
                        ),
                    );
                    thread::sleep(Duration::from_millis(1));
                }
            } else {
                let _ = resume.recv_timeout(Duration::from_secs(20));
            }
            pumping_tx.send(()).unwrap();
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                DispatchMessageW(&message);
            }
            DestroyWindow(hwnd).unwrap();
        });
        let (window_id, thread_id) = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        Self {
            window_id,
            thread_id,
            release,
            pumping,
            join: Some(join),
            control,
        }
    }

    fn resume(&self) {
        self.release.send(()).unwrap();
        self.pumping.recv_timeout(Duration::from_secs(5)).unwrap();
    }
}

impl Drop for PlacementOwner {
    fn drop(&mut self) {
        self.control.release();
        leopardwm_platform_win32::clear_suspected_oversize(self.window_id);
        let _ = self.release.send(());
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(join) = self.join.take() {
            join.join().unwrap();
        }
    }
}

fn placement_state(window_ids: &[u64]) -> AppState {
    let monitors = vec![MonitorInfo {
        id: 1,
        rect: Rect::new(0, 0, 800, 600),
        work_area: Rect::new(0, 0, 800, 560),
        is_primary: true,
        device_name: "DISPLAY1".to_string(),
        scale_factor: 1.0,
    }];
    let mut config = test_config();
    config.appearance.active_border = false;
    config.animation.layout_duration_ms = 0;
    config.animation.scroll_duration_ms = 0;
    let mut state = AppState::new_with_config(config, monitors.clone());
    state.reduce_motion = true;
    state.paused = false;
    state.injected_display_monitors = Some(monitors);
    for &hwnd in window_ids {
        state.workspaces.get_mut(&1).unwrap()[0]
            .insert_window(hwnd, Some(320))
            .unwrap();
        state
            .injected_window_info
            .insert(hwnd, make_test_window_info(hwnd));
    }
    state
}

fn pending_landing_recheck_state(owner: &PlacementOwner) -> (AppState, Vec<WindowPlacement>) {
    let mut state = placement_state(&[owner.window_id]);
    let placements = state.workspaces[&1][0].compute_placements_animated(state.layout_viewport(1));
    let placements = state.apply_physical_projection(placements);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    let landing =
        leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, true)
            .unwrap();
    assert!(landing.landings[0].measurement_deferred);
    assert!(!landing.landings[0].failed);
    let (request_id, invalidation_id) = state.physical_request_ids();
    state.consume_physical_landings(request_id, invalidation_id, &landing.landings, &[]);
    assert!(state.deferred_physical_landings.contains(&owner.window_id));
    (state, placements)
}

fn wait_for_fixture(mut ready: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while !ready() {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
    true
}

fn classify_non_draining_owner(owner: &PlacementOwner) -> (AppState, Vec<WindowPlacement>) {
    let (state, placements) = pending_landing_recheck_state(owner);
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(2),
    ));
    (state, placements)
}

#[test]
fn test_marked_mover_serves_both_slow_owners_during_continuous_frames() {
    if run_in_placement_test_process("tests::display_change_regression::test_marked_mover_serves_both_slow_owners_during_continuous_frames") { return; }
    let first =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let second =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&first);
    let (_, second_placements) = classify_non_draining_owner(&second);
    placements.extend(second_placements);
    for owner in [&first, &second] {
        owner.control.moves.lock().unwrap().clear();
        owner.control.move_delay_ms.store(40, Ordering::SeqCst);
    }
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        for placement in &mut placements {
            placement.rect.x += 1;
        }
        leopardwm_platform_win32::apply_placements(
            &placements,
            &state.platform_config,
            Some(&mut cache),
            false,
        )
        .unwrap();
        thread::sleep(Duration::from_millis(4));
    }
    for owner in [&first, &second] {
        owner.control.move_delay_ms.store(0, Ordering::SeqCst);
        assert!(
            owner.control.moves.lock().unwrap().len() >= 3,
            "one slow owner was starved of intermediate frames"
        );
    }
}

#[test]
fn test_visibility_recovery_stops_pending_and_late_mover_frames() {
    if run_in_placement_test_process("tests::display_change_regression::test_visibility_recovery_stops_pending_and_late_mover_frames") { return; }
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let before = leopardwm_platform_win32::get_window_chrome_rect(owner.window_id).unwrap();
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    placements[0].rect.x = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD;
    placements[0].visibility = Visibility::OffScreenLeft;
    owner.control.block_next.store(true, Ordering::SeqCst);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    placements[0].rect = Rect::new(240, 48, 160, 120);
    placements[0].visibility = Visibility::Visible;
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    let started = Instant::now();
    leopardwm_platform_win32::dwm_uncloak_all();
    leopardwm_platform_win32::restore_windows_moved_offscreen(&[owner.window_id]).unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "recovery waited for a held mover call"
    );
    cache.prepare_frame();
    leopardwm_platform_win32::dwm_uncloak_all();
    placements[0].rect.x = 360;
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    owner.control.release();
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id) == Some(before),
        Duration::from_secs(2)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(before),
        "a delayed mover frame undid visibility recovery"
    );
    let moves = owner.control.moves.lock().unwrap();
    assert!(
        !moves.contains(&(240, 48)),
        "pending frame survived recovery"
    );
    assert!(
        !moves.contains(&(360, 48)),
        "mover accepted a frame prepared before recovery"
    );
    drop(moves);
    placements[0].rect.x = 480;
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
                == Some(placements[0].rect),
            Duration::from_secs(2),
        ),
        "marked owner did not move after recovery resumed"
    );
}

#[test]
fn test_frame_prepared_before_landing_cannot_move_after_landing() {
    if run_in_placement_test_process("tests::display_change_regression::test_frame_prepared_before_landing_cannot_move_after_landing") { return; }
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let fresh =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (_, mut fresh_placements) = classify_non_draining_owner(&fresh);
    fresh_placements[0].rect.x += 300;
    let mut stale_cache = leopardwm_platform_win32::PlacementCache::new();
    stale_cache.prepare_frame();
    let mut stale = placements.clone();
    stale[0].rect.x += 100;
    placements[0].rect.x += 200;
    let landing =
        leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, true)
            .unwrap();
    assert!(!landing.landings[0].measurement_deferred);
    owner.control.block_next.store(true, Ordering::SeqCst);
    leopardwm_platform_win32::apply_placements(
        &stale,
        &state.platform_config,
        Some(&mut stale_cache),
        false,
    )
    .unwrap();
    // A fresh owner's delivered frame fences the FIFO without a sleep-based absence assertion.
    let mut fresh_cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &fresh_placements,
        &state.platform_config,
        Some(&mut fresh_cache),
        false,
    )
    .unwrap();
    let delivered = wait_for_fixture(
        || {
            leopardwm_platform_win32::get_window_chrome_rect(fresh.window_id)
                == Some(fresh_placements[0].rect)
        },
        Duration::from_secs(2),
    );
    let stale_entered = owner.control.entered.load(Ordering::SeqCst);
    owner.control.release();
    assert!(
        !stale_entered,
        "a frame prepared before the seal entered the mover"
    );
    assert!(delivered);
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
}

#[test]
fn test_unrelated_landing_preserves_pending_marked_frame_and_parked_record() {
    if run_in_placement_test_process("tests::display_change_regression::test_unrelated_landing_preserves_pending_marked_frame_and_parked_record") { return; }
    let blocker =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let parked = PlacementOwner::spawn();
    parked.resume();
    let (state, mut blocking) = classify_non_draining_owner(&blocker);
    let (_, mut pending) = classify_non_draining_owner(&owner);
    let mut parked_placement = pending.clone();
    parked_placement[0].window_id = parked.window_id;
    parked_placement[0].rect = Rect::new(
        leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD,
        0,
        0,
        0,
    );
    parked_placement[0].visibility = Visibility::OffScreenLeft;
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &parked_placement,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::offscreen_placement_matches(parked.window_id) == Some(true),
        Duration::from_secs(2)
    ));
    blocking[0].rect.x += 100;
    blocker.control.block_next.store(true, Ordering::SeqCst);
    leopardwm_platform_win32::apply_placements(
        &blocking,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || blocker.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    pending[0].rect.x += 200;
    leopardwm_platform_win32::apply_placements(
        &pending,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    cache.prepare_frame();
    leopardwm_platform_win32::apply_placements(&blocking, &state.platform_config, None, true)
        .unwrap();
    leopardwm_platform_win32::apply_placements(
        &parked_placement,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    cache.prepare_frame();
    assert_eq!(
        leopardwm_platform_win32::offscreen_placement_matches(parked.window_id),
        Some(true)
    );
    blocker.control.release();
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
                == Some(pending[0].rect),
            Duration::from_secs(2)
        ),
        "unrelated landing discarded the pending marked frame"
    );
}

#[test]
fn test_late_cancelled_move_skips_undo_when_owner_stops_answering() {
    if run_in_placement_test_process("tests::display_change_regression::test_late_cancelled_move_skips_undo_when_owner_stops_answering") { return; }
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let barrier =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (_, mut barrier_placements) = classify_non_draining_owner(&barrier);
    placements[0].rect.x += 100;
    owner.control.block_next.store(true, Ordering::SeqCst);
    owner.control.pause_after_move.store(true, Ordering::SeqCst);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    leopardwm_platform_win32::stop_placement_mover();
    owner.control.release();
    assert!(wait_for_fixture(
        || owner.control.pump_paused.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    barrier_placements[0].rect.x += 200;
    leopardwm_platform_win32::apply_placements(
        &barrier_placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(barrier.window_id)
                == Some(barrier_placements[0].rect),
            Duration::from_secs(2)
        ),
        "unresponsive recovery undo blocked the mover"
    );
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
    assert!(!leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
}

#[test]
fn test_cascade_accepts_in_flight_mover_and_recovers_late_offscreen_move() {
    if run_in_placement_test_process("tests::display_change_regression::test_cascade_accepts_in_flight_mover_and_recovers_late_offscreen_move") { return; }
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let work_area = leopardwm_platform_win32::get_primary_monitor()
        .unwrap()
        .work_area;
    let cascade_height = (work_area.height as f64 * 0.5) as i32;
    let cascade_rect = Rect::new(
        work_area.x,
        work_area.y,
        (cascade_height as f64 * 1.33) as i32,
        cascade_height,
    );
    placements[0].rect.x = leopardwm_platform_win32::MOVE_OFFSCREEN_SENTINEL_COORD;
    placements[0].visibility = Visibility::OffScreenLeft;
    owner.control.block_next.store(true, Ordering::SeqCst);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    let control = Arc::clone(&owner.control);
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_millis(300));
        control.release();
    });
    let result = leopardwm_platform_win32::cascade_windows(&[owner.window_id]);
    release.join().unwrap();
    result.unwrap();
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
                == Some(cascade_rect),
            Duration::from_secs(2)
        ),
        "late mover frame did not converge on the cascade rectangle"
    );
}

#[test]
fn test_newer_landing_supersedes_cancelled_in_flight_move() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_newer_landing_supersedes_cancelled_in_flight_move",
    ) {
        return;
    }
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let barrier =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (_, mut barrier_placements) = classify_non_draining_owner(&barrier);
    placements[0].rect.x += 100;
    owner.control.block_next.store(true, Ordering::SeqCst);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    leopardwm_platform_win32::stop_placement_mover();
    placements[0].rect.x += 200;
    let landing =
        leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, true)
            .unwrap();
    assert!(landing.landings[0].measurement_deferred);
    owner.control.release();
    barrier_placements[0].rect.x += 300;
    leopardwm_platform_win32::apply_placements(
        &barrier_placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::get_window_chrome_rect(barrier.window_id)
            == Some(barrier_placements[0].rect),
        Duration::from_secs(2)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect),
        "cancel-only undo overwrote the newer authoritative landing"
    );
}

#[test]
fn test_non_draining_owner_receives_animation_frames_without_blocking_caller() {
    if run_in_placement_test_process("tests::display_change_regression::test_non_draining_owner_receives_animation_frames_without_blocking_caller") { return; }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    placements[0].rect.x += 100;
    owner.control.block_next.store(true, Ordering::SeqCst);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    let started = Instant::now();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(
        wait_for_fixture(
            || owner.control.entered.load(Ordering::SeqCst),
            Duration::from_secs(2)
        ),
        "animation frame never reached the non-draining owner"
    );
    placements[0].rect.x += 100;
    let superseded_origin = (placements[0].rect.x, placements[0].rect.y);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    placements[0].rect.x += 100;
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    owner.control.release();
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
            == Some(placements[0].rect),
        Duration::from_secs(2)
    ));
    assert!(
        !owner
            .control
            .moves
            .lock()
            .unwrap()
            .contains(&superseded_origin),
        "superseded pending frame was applied"
    );
}

#[test]
fn test_marked_owner_lands_without_latest_async_drain_wait() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_marked_owner_lands_without_latest_async_drain_wait",
    ) {
        return;
    }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    wait_for_fixture(
        || {
            leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
                == Some(placements[0].rect)
                && !leopardwm_platform_win32::pending_async_position(owner.window_id)
        },
        Duration::from_millis(150),
    );
    placements[0].rect.x += 100;
    let started = Instant::now();
    let landing =
        leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, true)
            .unwrap();
    assert!(
        !landing.landings[0].measurement_deferred,
        "marked landing still waits for the latest async post"
    );
    assert!(started.elapsed() < Duration::from_millis(200));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
    let hwnd = HWND(owner.window_id as usize as *mut _);
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd),
            windows::Win32::UI::WindowsAndMessaging::WM_APP + 42,
            WPARAM(0),
            LPARAM(0),
        )
        .unwrap();
    }
    owner.resume();
    assert!(wait_for_fixture(
        || unsafe {
            !GetPropW(hwnd, w!("LeopardWMFixtureQueueDrained"))
                .0
                .is_null()
        },
        Duration::from_secs(2)
    ));
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
                == Some(placements[0].rect),
            Duration::from_secs(2),
        ),
        "a matching synchronous frame must not discard the pre-classification queue fence"
    );
}

#[test]
fn test_marked_landing_seals_stale_frame_and_defers_in_flight_move() {
    if run_in_placement_test_process("tests::display_change_regression::test_marked_landing_seals_stale_frame_and_defers_in_flight_move") { return; }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (mut state, mut placements) = classify_non_draining_owner(&owner);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    placements[0].rect.x += 100;
    owner.control.block_next.store(true, Ordering::SeqCst);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        wait_for_fixture(
            || owner.control.entered.load(Ordering::SeqCst),
            Duration::from_secs(2)
        ),
        "frame did not enter the synchronous mover"
    );
    assert!(leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    placements[0].rect.x += 100;
    let stale_origin = (placements[0].rect.x, placements[0].rect.y);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    placements[0].rect.x -= 200;
    let started = Instant::now();
    let landing =
        leopardwm_platform_win32::apply_placements(&placements, &state.platform_config, None, true)
            .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "landing waited for the mover"
    );
    assert!(landing.landings[0].measurement_deferred);
    let (request, invalidation) = state.physical_request_ids();
    state.consume_physical_landings(request, invalidation, &landing.landings, &[]);
    owner.control.release();
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(2)
    ));
    state.moved_or_resized_suppression.clear();
    assert!(
        !state.recheck_deferred_window_moves(),
        "the mover-completed landing should not need another layout apply"
    );
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
    assert!(
        !owner.control.moves.lock().unwrap().contains(&stale_origin),
        "sealed frame was applied after landing"
    );
    assert!(!state.deferred_physical_landings.contains(&owner.window_id));
}

#[test]
fn test_non_draining_mover_drops_targets_when_owner_stops_answering() {
    if run_in_placement_test_process("tests::display_change_regression::test_non_draining_mover_drops_targets_when_owner_stops_answering") { return; }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = classify_non_draining_owner(&owner);
    let before = leopardwm_platform_win32::get_window_chrome_rect(owner.window_id);
    owner.control.pause_pump.store(true, Ordering::SeqCst);
    assert!(wait_for_fixture(
        || owner.control.pump_paused.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    let hwnd = HWND(owner.window_id as usize as *mut _);
    assert!(wait_for_fixture(
        || unsafe {
            windows::Win32::UI::WindowsAndMessaging::SendMessageTimeoutW(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::WM_NULL,
                WPARAM(0),
                LPARAM(0),
                windows::Win32::UI::WindowsAndMessaging::SMTO_ABORTIFHUNG
                    | windows::Win32::UI::WindowsAndMessaging::SMTO_BLOCK,
                10,
                None,
            )
            .0 == 0
        },
        Duration::from_secs(1)
    ));
    // Let the successful owner-answer cache expire before submitting this frame.
    let expires = Instant::now() + Duration::from_millis(260);
    assert!(wait_for_fixture(
        || Instant::now() >= expires,
        Duration::from_secs(1)
    ));
    owner.control.block_next.store(true, Ordering::SeqCst);
    placements[0].rect.x += 100;
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        wait_for_fixture(
            || leopardwm_platform_win32::pending_async_position(owner.window_id),
            Duration::from_secs(2)
        ),
        "mover never started the unanswered owner probe"
    );
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(2)
    ));
    owner.control.pause_pump.store(false, Ordering::SeqCst);
    assert!(wait_for_fixture(
        || !owner.control.pump_paused.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    let barrier = unsafe {
        windows::Win32::UI::WindowsAndMessaging::SendMessageTimeoutW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::WM_NULL,
            WPARAM(0),
            LPARAM(0),
            windows::Win32::UI::WindowsAndMessaging::SMTO_ABORTIFHUNG
                | windows::Win32::UI::WindowsAndMessaging::SMTO_BLOCK,
            100,
            None,
        )
        .0
    };
    assert_ne!(barrier, 0, "mover blocked on a non-answering owner");
    assert!(!owner.control.entered.load(Ordering::SeqCst));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        before
    );
}

#[test]
fn test_managed_departure_forgets_queued_move_fallback_and_pending_frame() {
    if run_in_placement_test_process("tests::display_change_regression::test_managed_departure_forgets_queued_move_fallback_and_pending_frame") { return; }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (mut state, mut placements) = classify_non_draining_owner(&owner);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    placements[0].rect.x += 100;
    let first_target = placements[0].rect;
    owner.control.block_next.store(true, Ordering::SeqCst);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(wait_for_fixture(
        || owner.control.entered.load(Ordering::SeqCst),
        Duration::from_secs(2)
    ));
    placements[0].rect.x += 100;
    let forgotten_origin = (placements[0].rect.x, placements[0].rect.y);
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    state.take_managed_lifetime_token(owner.window_id);
    assert!(
        leopardwm_platform_win32::pending_async_position(owner.window_id),
        "forgetting must not hide an already executing call"
    );
    owner.control.release();
    assert!(wait_for_fixture(
        || !leopardwm_platform_win32::pending_async_position(owner.window_id),
        Duration::from_secs(2)
    ));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(first_target)
    );
    assert!(!owner
        .control
        .moves
        .lock()
        .unwrap()
        .contains(&forgotten_origin));
    placements[0].rect.x += 100;
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        leopardwm_platform_win32::pending_async_position(owner.window_id),
        "new lifetime inherited the queued-move fallback"
    );
    let landing = leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    assert!(landing.landings[0].measurement_deferred);
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(first_target)
    );
}

#[test]
fn test_unmarked_owner_keeps_async_animation_delivery() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_unmarked_owner_keeps_async_animation_delivery",
    ) {
        return;
    }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let owner = PlacementOwner::spawn();
    let state = placement_state(&[owner.window_id]);
    let placements = state.workspaces[&1][0].compute_placements_animated(state.layout_viewport(1));
    let before = leopardwm_platform_win32::get_window_chrome_rect(owner.window_id);
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    let started = Instant::now();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_millis(200));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        before
    );
    assert!(leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    owner.resume();
    assert!(wait_for_fixture(
        || leopardwm_platform_win32::get_window_chrome_rect(owner.window_id)
            == Some(placements[0].rect),
        Duration::from_secs(2)
    ));
}

#[test]
fn test_deferred_replay_survives_a_pending_landing() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = PlacementOwner::spawn();
    let (mut state, _) = pending_landing_recheck_state(&owner);
    state.deferred_moved_or_resized.insert(owner.window_id);
    state.applying_layout = true;
    let before = state.physical_request_seq;
    assert!(leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before);
    state.applying_layout = false;
    state.moved_or_resized_suppression.clear();
    assert!(leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
    assert!(!state.window_move_recheck_attempts[&owner.window_id].gave_up);
}

#[test]
fn test_deferred_landing_provenance_survives_suppression_and_another_correction() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let pending = PlacementOwner::spawn();
    let correction = OffscreenResizeOwner::new();
    let (mut state, _) = pending_landing_recheck_state(&pending);
    state.workspaces.get_mut(&1).unwrap()[0]
        .insert_window(correction.hwnd, Some(300))
        .unwrap();
    let placements = state.workspaces[&1][0].compute_placements_animated(state.layout_viewport(1));
    let placements = state.apply_physical_projection(placements);
    let landings: Vec<_> = placements
        .iter()
        .map(|placement| leopardwm_platform_win32::PlacementLanding {
            window_id: placement.window_id,
            requested_rect: placement.rect,
            requested_visibility: placement.visibility,
            actual_visible_rect: None,
            actual_outer_rect: None,
            failed: false,
            unreadable: false,
            measurement_deferred: true,
        })
        .collect();
    let (request_id, invalidation_id) = state.physical_request_ids();
    state.consume_physical_landings(request_id, invalidation_id, &landings, &[]);
    state.applying_layout = true;
    let before = state.physical_request_seq;
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before);
    assert!(!state.deferred_moved_or_resized.contains(&correction.hwnd));
    assert!(!state.deferred_moved_or_resized.contains(&pending.window_id));
    assert!(state.deferred_physical_landings.contains(&correction.hwnd));
    assert!(state
        .deferred_physical_landings
        .contains(&pending.window_id));
    state.applying_layout = false;
    state.moved_or_resized_suppression.clear();
    assert!(leopardwm_platform_win32::pending_async_position(
        pending.window_id
    ));
    assert!(state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
    assert_eq!(
        correction.rect(),
        placements
            .iter()
            .find(|placement| placement.window_id == correction.hwnd)
            .unwrap()
            .rect
    );
    assert!(!state.deferred_moved_or_resized.contains(&pending.window_id));
    assert!(state
        .deferred_physical_landings
        .contains(&pending.window_id));
    state.moved_or_resized_suppression.clear();
    assert!(leopardwm_platform_win32::pending_async_position(
        pending.window_id
    ));
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
    assert!(!state
        .window_move_recheck_attempts
        .contains_key(&pending.window_id));
}

#[test]
fn test_deferred_landing_corrects_responsive_non_draining_owner_promptly() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (mut state, placements) = pending_landing_recheck_state(&owner);
    let landing_at = Instant::now();
    let before = state.physical_request_seq;
    let mut corrections = 0;
    while landing_at.elapsed() < Duration::from_secs(2) {
        state.moved_or_resized_suppression.clear();
        if state.recheck_deferred_window_moves() {
            corrections += 1;
            break;
        }
        assert_eq!(state.physical_request_seq, before);
        assert!(!state
            .window_move_recheck_attempts
            .contains_key(&owner.window_id));
        thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        corrections, 1,
        "responsive owner was not corrected within 2 s"
    );
    assert!(landing_at.elapsed() < Duration::from_secs(1));
    assert_eq!(state.physical_request_seq, before + 1);
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
    state.moved_or_resized_suppression.clear();
    assert!(!state.recheck_deferred_window_moves());
    assert_eq!(state.physical_request_seq, before + 1);
}

#[test]
fn test_deferred_landing_waits_for_latest_async_submission() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = pending_landing_recheck_state(&owner);
    thread::sleep(Duration::from_millis(350));
    placements[0].rect.x += 100;
    let mut cache = leopardwm_platform_win32::PlacementCache::new();
    leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        Some(&mut cache),
        false,
    )
    .unwrap();
    assert!(
        leopardwm_platform_win32::pending_async_position(owner.window_id),
        "a fresh frame must remain pending even when the first frame is old"
    );
    let landing = leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    assert!(landing.landings[0].measurement_deferred);
    assert_ne!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
}

#[test]
fn test_deferred_landing_preserves_latest_endpoint_when_owner_drains_later() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner =
        PlacementOwner::spawn_with_message_mode(Rect::new(48, 48, 160, 120), 0, false, true);
    let (state, mut placements) = pending_landing_recheck_state(&owner);
    thread::sleep(Duration::from_millis(350));
    assert!(!leopardwm_platform_win32::pending_async_position(
        owner.window_id
    ));
    placements[0].rect.x += 100;
    let correction = leopardwm_platform_win32::apply_placements(
        &placements,
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    assert!(!correction.landings[0].measurement_deferred);
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect)
    );
    let hwnd = HWND(owner.window_id as usize as *mut _);
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd),
            windows::Win32::UI::WindowsAndMessaging::WM_APP + 42,
            WPARAM(0),
            LPARAM(0),
        )
        .unwrap();
    }
    owner.resume();
    let deadline = Instant::now() + Duration::from_secs(2);
    while unsafe {
        GetPropW(hwnd, w!("LeopardWMFixtureQueueDrained"))
            .0
            .is_null()
    } {
        assert!(
            Instant::now() < deadline,
            "owner did not drain its posted queue"
        );
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
        Some(placements[0].rect),
        "old queued frames must not overtake the latest endpoint"
    );
}

#[test]
fn test_deferred_landing_keeps_blocked_owner_pending_after_short_wait() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owner = PlacementOwner::spawn();
    let (mut state, _) = pending_landing_recheck_state(&owner);
    let before = state.physical_request_seq;
    let original_rect = leopardwm_platform_win32::get_window_chrome_rect(owner.window_id);
    thread::sleep(Duration::from_millis(350));
    for _ in 0..3 {
        assert!(leopardwm_platform_win32::pending_async_position(
            owner.window_id
        ));
        state.moved_or_resized_suppression.clear();
        assert!(!state.recheck_deferred_window_moves());
        assert_eq!(state.physical_request_seq, before);
        assert!(!state
            .window_move_recheck_attempts
            .contains_key(&owner.window_id));
        assert!(state.deferred_physical_landings.contains(&owner.window_id));
        assert_eq!(
            leopardwm_platform_win32::get_window_chrome_rect(owner.window_id),
            original_rect
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn test_display_change_places_responsive_windows_while_owner_is_not_pumping() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn();
    let responsive = PlacementOwner::spawn();
    responsive.resume();
    let mut state = placement_state(&[blocked.window_id, responsive.window_id]);
    let before = leopardwm_platform_win32::get_window_visible_rect(blocked.window_id).unwrap();
    let start = Instant::now();
    state.handle_window_event(WindowEvent::DisplayChange);
    if let Some(retry) = state.display_change_apply_retry.as_ref() {
        let generation = retry.generation;
        let _ = state.run_display_change_apply_retry(generation);
    }
    let remained_active = !state.paused && state.pending_apply_workers.is_empty();
    let elapsed = start.elapsed();
    if !remained_active {
        blocked.resume();
        join_pending_test_apply_workers(&mut state);
    }
    assert!(
        remained_active,
        "display reconciliation paused behind a non-pumping owner"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "display apply waited on owner: {elapsed:?}"
    );
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(blocked.window_id).unwrap(),
        before
    );
    let expected: HashMap<_, _> = state.workspaces[&1][0]
        .compute_placements_animated(state.layout_viewport(1))
        .into_iter()
        .map(|placement| (placement.window_id, placement.rect))
        .collect();
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(responsive.window_id),
        Some(expected[&responsive.window_id])
    );
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);

    // Outlive both the production apply/retry interval and the ordinary async expiry.
    thread::sleep(Duration::from_millis(10_100).saturating_sub(start.elapsed()));
    state.apply_layout().unwrap();
    assert!(!state.paused);
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
    blocked.resume();
    let deadline = Instant::now() + Duration::from_secs(2);
    while leopardwm_platform_win32::get_window_visible_rect(blocked.window_id)
        != Some(expected[&blocked.window_id])
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(blocked.window_id),
        Some(expected[&blocked.window_id])
    );
    state.apply_layout().unwrap();
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
    state.apply_layout().unwrap();
    assert!(state.last_physical_presentations[&blocked.window_id].confirmed);
    assert!(!state.paused);
}

#[test]
fn test_display_change_retry_defers_non_pumping_owner() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn();
    let responsive = PlacementOwner::spawn();
    responsive.resume();
    let mut state = placement_state(&[blocked.window_id, responsive.window_id]);
    state.layout_apply_timeout = Duration::from_millis(10);
    state.injected_apply_placements_behavior = Some(TestApplyPlacementsBehavior::SleepAndSucceed(
        Duration::from_millis(50),
    ));
    state.handle_window_event(WindowEvent::DisplayChange);
    let generation = pending_display_change_retry_generation(&state);
    for worker in state.pending_apply_workers.drain(..) {
        worker.join().unwrap();
    }
    state.injected_apply_placements_behavior = None;
    state.layout_apply_timeout = Duration::from_secs(5);
    let start = Instant::now();
    let result = state.run_display_change_apply_retry(generation);
    let elapsed = start.elapsed();
    let remained_active = result.is_ok() && !state.paused && state.pending_apply_workers.is_empty();
    if !remained_active {
        blocked.resume();
        join_pending_test_apply_workers(&mut state);
    }
    assert!(
        remained_active,
        "display-change retry blocked or paused: {result:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "retry waited on owner: {elapsed:?}"
    );
    let expected = state.workspaces[&1][0]
        .compute_placements_animated(state.layout_viewport(1))
        .into_iter()
        .find(|placement| placement.window_id == responsive.window_id)
        .unwrap()
        .rect;
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(responsive.window_id),
        Some(expected)
    );
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
    assert!(state.display_change_apply_retry.is_none());
}

#[test]
fn test_display_change_size_only_deferral_waits_for_owner() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let template = placement_state(&[100]);
    let expected =
        template.workspaces[&1][0].compute_placements_animated(template.layout_viewport(1))[0].rect;
    let before = Rect::new(
        expected.x,
        expected.y,
        expected.width - 20,
        expected.height - 20,
    );
    let blocked = PlacementOwner::spawn_at(before);
    let mut state = placement_state(&[blocked.window_id]);
    state.handle_window_event(WindowEvent::DisplayChange);
    assert!(!state.paused);
    let start = Instant::now();
    let result = state.apply_layout();
    let elapsed = start.elapsed();
    let stayed_deferred = result.is_ok() && !state.paused;
    if !stayed_deferred {
        blocked.resume();
        join_pending_test_apply_workers(&mut state);
    }
    assert!(
        stayed_deferred,
        "same-origin async resize drained before owner pumped: {result:?}"
    );
    assert!(elapsed < Duration::from_secs(2));
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(blocked.window_id),
        Some(before)
    );
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
    blocked.resume();
    let deadline = Instant::now() + Duration::from_secs(2);
    while leopardwm_platform_win32::get_window_visible_rect(blocked.window_id) != Some(expected)
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(blocked.window_id),
        Some(expected)
    );
    state.apply_layout().unwrap();
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
    state.apply_layout().unwrap();
    assert!(state.last_physical_presentations[&blocked.window_id].confirmed);
}

#[test]
fn test_owner_wait_unchanged_rect_does_not_drain_while_blocked() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let template = placement_state(&[100]);
    let expected =
        template.workspaces[&1][0].compute_placements_animated(template.layout_viewport(1))[0].rect;
    let blocked = PlacementOwner::spawn_at(expected);
    let mut state = placement_state(&[blocked.window_id]);
    state.handle_window_event(WindowEvent::DisplayChange);
    let result = state.apply_layout();
    let active = result.is_ok() && !state.paused;
    if !active {
        blocked.resume();
        join_pending_test_apply_workers(&mut state);
    }
    assert!(
        active,
        "matching geometry drained a blocked owner's deferral: {result:?}"
    );
    assert!(!state.last_physical_presentations[&blocked.window_id].confirmed);
}

#[test]
fn test_owner_wait_clamped_size_returns_to_feedback_after_resume() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn_with(Rect::new(48, 48, 160, 120), 360, false);
    let mut state = placement_state(&[blocked.window_id]);
    state.handle_window_event(WindowEvent::DisplayChange);
    blocked.resume();
    let deadline = Instant::now() + Duration::from_secs(2);
    while leopardwm_platform_win32::get_window_visible_rect(blocked.window_id)
        .unwrap()
        .width
        != 360
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(blocked.window_id)
            .unwrap()
            .width,
        360
    );
    for _ in 0..3 {
        state.apply_layout().unwrap();
    }
    assert!(
        state.last_physical_presentations[&blocked.window_id].confirmed,
        "a pumping owner that clamps its size stayed measurement-deferred"
    );
    let placed =
        state.workspaces[&1][0].compute_placements_animated(state.layout_viewport(1))[0].rect;
    assert!(
        placed.width >= 360,
        "native width feedback was never restored: {placed:?}"
    );
}

fn fixture_placement(window_id: u64, visibility: Visibility) -> WindowPlacement {
    WindowPlacement {
        window_id,
        rect: if visibility == Visibility::Visible {
            Rect::new(32, 32, 320, 240)
        } else {
            Rect::new(-10_000, -10_000, 320, 240)
        },
        visibility,
        column_index: 0,
    }
}

#[test]
fn test_owner_wait_offscreen_deferral_drains_after_resume() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn();
    let state = placement_state(&[]);
    let placement = fixture_placement(blocked.window_id, Visibility::OffScreenLeft);
    let (first, _) = leopardwm_platform_win32::apply_display_change_placements(
        std::slice::from_ref(&placement),
        &state.platform_config,
        false,
    )
    .unwrap();
    assert!(first.landings[0].measurement_deferred);
    blocked.resume();
    let mut last = first;
    for _ in 0..3 {
        last = leopardwm_platform_win32::apply_placements(
            std::slice::from_ref(&placement),
            &state.platform_config,
            None,
            false,
        )
        .unwrap();
    }
    assert!(
        !last.landings[0].measurement_deferred,
        "SWP_NOSIZE off-screen deferral never drained after the owner pumped"
    );
    assert!(!last.landings[0].failed);
}

#[test]
fn test_owner_wait_parked_maximized_recovery_is_classified_before_positioning() {
    if run_in_placement_test_process(
        "tests::display_change_regression::test_owner_wait_parked_maximized_recovery_is_classified_before_positioning",
    ) {
        return;
    }
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn_with(Rect::new(48, 48, 320, 240), 0, true);
    assert!(leopardwm_platform_win32::is_placement_parked(
        blocked.window_id
    ));
    let state = placement_state(&[]);
    let placement = fixture_placement(blocked.window_id, Visibility::Visible);
    let config = state.platform_config.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let worker_placement = placement.clone();
    let worker = thread::spawn(move || {
        let result = leopardwm_platform_win32::apply_display_change_placements(
            std::slice::from_ref(&worker_placement),
            &config,
            false,
        );
        done_tx.send(result).unwrap();
    });
    let fast = done_rx.recv_timeout(Duration::from_millis(400));
    let returned_while_blocked = fast.is_ok();
    blocked.resume();
    let (result, deferred) = fast
        .unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap())
        .unwrap();
    worker.join().unwrap();
    assert!(
        returned_while_blocked,
        "parked-maximized recovery positioned synchronously before probing"
    );
    assert!(deferred.contains(&blocked.window_id));
    assert!(result.landings[0].measurement_deferred);
    for _ in 0..3 {
        let result = leopardwm_platform_win32::apply_placements(
            std::slice::from_ref(&placement),
            &state.platform_config,
            None,
            false,
        )
        .unwrap();
        if !result.landings[0].measurement_deferred {
            return;
        }
    }
    panic!("parked-maximized recovery did not drain after owner resumed");
}

#[test]
fn test_owner_wait_recovery_leaves_an_on_screen_target_untouched() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let monitor = leopardwm_platform_win32::get_primary_monitor().unwrap();
    let current = Rect::new(monitor.work_area.x + 48, monitor.work_area.y + 48, 160, 120);
    let targets = [
        Rect::new(current.x, current.y, 320, 240),
        Rect::new(monitor.work_area.x - 240, current.y, 320, 240),
    ];
    for rect in targets {
        let blocked = PlacementOwner::spawn_at(current);
        let state = placement_state(&[]);
        let mut placement = fixture_placement(blocked.window_id, Visibility::Visible);
        placement.rect = rect;
        let (_, deferred) = leopardwm_platform_win32::apply_display_change_placements(
            std::slice::from_ref(&placement),
            &state.platform_config,
            false,
        )
        .unwrap();
        assert!(deferred.contains(&blocked.window_id));
        let window_id = blocked.window_id;
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            done_tx
                .send(leopardwm_platform_win32::restore_windows_moved_offscreen(
                    &[window_id],
                ))
                .unwrap();
        });
        let fast = done_rx.recv_timeout(Duration::from_millis(400));
        let returned_while_blocked = fast.is_ok();
        blocked.resume();
        let restored = fast
            .unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .unwrap();
        worker.join().unwrap();
        assert!(
            returned_while_blocked,
            "recovery synchronously positioned an on-screen waiting owner"
        );
        assert_eq!(
            restored, 0,
            "recovery queued a move for an on-screen target"
        );
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            leopardwm_platform_win32::get_window_visible_rect(window_id),
            Some(rect),
            "recovery dragged an on-screen deferred window to the work-area origin"
        );
    }
}

#[test]
fn test_owner_wait_recovery_updates_size_before_a_size_preserving_park() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let primary = leopardwm_platform_win32::get_primary_monitor().unwrap();
    let monitors = leopardwm_platform_win32::enumerate_monitors().unwrap();
    let leftmost = monitors
        .iter()
        .min_by_key(|monitor| monitor.work_area.x)
        .unwrap();
    let current = Rect::new(primary.work_area.x + 48, primary.work_area.y + 48, 160, 120);
    let blocked = PlacementOwner::spawn_at(current);
    let window_id = blocked.window_id;
    let park_rect = Rect::new(
        leftmost.work_area.x - 240,
        leftmost.work_area.y + 48,
        320,
        240,
    );
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let state = placement_state(&[]);
        let mut resize = fixture_placement(window_id, Visibility::Visible);
        resize.rect = Rect::new(current.x, current.y, 320, 240);
        let (_, deferred) = leopardwm_platform_win32::apply_display_change_placements(
            &[resize],
            &state.platform_config,
            false,
        )
        .unwrap();
        leopardwm_platform_win32::queue_window_offscreen(window_id).unwrap();
        let first =
            leopardwm_platform_win32::restore_windows_moved_offscreen(&[window_id]).unwrap();
        let mut park = fixture_placement(window_id, Visibility::OffScreenLeft);
        park.rect = park_rect;
        leopardwm_platform_win32::apply_placements(&[park], &state.platform_config, None, false)
            .unwrap();
        let second =
            leopardwm_platform_win32::restore_windows_moved_offscreen(&[window_id]).unwrap();
        done_tx
            .send((deferred.contains(&window_id), first, second))
            .unwrap();
    });
    let fast = done_rx.recv_timeout(Duration::from_millis(600));
    let returned_while_blocked = fast.is_ok();
    blocked.resume();
    let (deferred, first, second) =
        fast.unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    worker.join().unwrap();
    assert!(
        returned_while_blocked,
        "park/recovery sequence blocked on the owner"
    );
    assert!(deferred);
    assert_eq!(first, 1);
    assert_eq!(
        second, 1,
        "recovery misclassified a size-preserving park using the pre-restore size"
    );
    let expected = Rect::new(primary.work_area.x, primary.work_area.y, 160, 120);
    let deadline = Instant::now() + Duration::from_secs(2);
    while leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap() != expected
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap(),
        expected,
        "the size-preserving park overtook the final visible restore"
    );
}

#[test]
fn test_owner_wait_recovery_follows_a_park_that_has_not_landed() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for display_apply in [false, true] {
        let blocked = PlacementOwner::spawn();
        let state = placement_state(&[]);
        if display_apply {
            let placement = fixture_placement(blocked.window_id, Visibility::OffScreenLeft);
            leopardwm_platform_win32::apply_display_change_placements(
                &[placement],
                &state.platform_config,
                false,
            )
            .unwrap();
        } else {
            leopardwm_platform_win32::queue_window_offscreen(blocked.window_id).unwrap();
        }
        assert!(!leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
            &leopardwm_platform_win32::get_window_chrome_rect(blocked.window_id).unwrap()
        ));
        let window_id = blocked.window_id;
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            done_tx
                .send(leopardwm_platform_win32::restore_windows_moved_offscreen(
                    &[window_id],
                ))
                .unwrap();
        });
        let fast = done_rx.recv_timeout(Duration::from_millis(400));
        let returned_while_blocked = fast.is_ok();
        blocked.resume();
        let restored = fast
            .unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap())
            .unwrap();
        worker.join().unwrap();
        assert!(
            returned_while_blocked,
            "recovery synchronously positioned a waiting owner"
        );
        assert_eq!(
            restored, 1,
            "recovery skipped a queued park before it landed"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let rect = leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap();
            if rect.x >= 0 && rect.y >= 0 {
                // Allow the queued park and recovery to both run before checking final geometry.
                thread::sleep(Duration::from_millis(100));
                let final_rect =
                    leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap();
                assert!(final_rect.x >= 0 && final_rect.y >= 0);
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let final_rect = leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap();
        assert!(
            final_rect.x >= 0 && final_rect.y >= 0,
            "queued park overtook recovery"
        );
    }
}

#[test]
fn test_owner_wait_omitted_window_is_probed_after_resume() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn();
    leopardwm_platform_win32::queue_window_offscreen(blocked.window_id).unwrap();
    blocked.resume();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
        &leopardwm_platform_win32::get_window_chrome_rect(blocked.window_id).unwrap(),
    ) && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
        &leopardwm_platform_win32::get_window_chrome_rect(blocked.window_id).unwrap()
    ));
    let state = placement_state(&[]);
    leopardwm_platform_win32::apply_placements(&[], &state.platform_config, None, false).unwrap();
    let placement = fixture_placement(blocked.window_id, Visibility::OffScreenLeft);
    let result = leopardwm_platform_win32::apply_placements(
        &[placement],
        &state.platform_config,
        None,
        false,
    )
    .unwrap();
    assert!(
        !result.landings[0].measurement_deferred,
        "an omitted responding owner retained its owner-wait marker"
    );
}

#[test]
fn test_owner_probe_budget_keeps_unprobed_windows_deferred_without_reporting_them() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let owners: Vec<_> = (0..16).map(|_| PlacementOwner::spawn()).collect();
    let placements: Vec<_> = owners
        .iter()
        .map(|owner| {
            let mut placement = fixture_placement(owner.window_id, Visibility::Visible);
            placement.rect.x = 48;
            placement.rect.y = 48;
            placement
        })
        .collect();
    let state = placement_state(&[]);
    let started = Instant::now();
    let (result, unresponsive) = leopardwm_platform_win32::apply_display_change_placements(
        &placements,
        &state.platform_config,
        false,
    )
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(result
        .landings
        .iter()
        .all(|landing| landing.measurement_deferred));
    assert!(!unresponsive.is_empty());
    assert!(
        unresponsive.len() < owners.len(),
        "budget-exhausted owners were reported as unresponsive without being probed"
    );
    let unprobed = owners
        .iter()
        .find(|owner| !unresponsive.contains(&owner.window_id))
        .unwrap();
    let window_id = unprobed.window_id;
    let config = state.platform_config.clone();
    let mut placement = fixture_placement(window_id, Visibility::Visible);
    placement.rect = Rect::new(48, 48, 360, 240);
    let (done_tx, done_rx) = mpsc::channel();
    let worker_placement = placement.clone();
    let worker = thread::spawn(move || {
        let mut cache = leopardwm_platform_win32::PlacementCache::new();
        leopardwm_platform_win32::apply_placements(
            std::slice::from_ref(&worker_placement),
            &config,
            Some(&mut cache),
            false,
        )
        .unwrap();
        let result =
            leopardwm_platform_win32::apply_placements(&[worker_placement], &config, None, false)
                .unwrap();
        done_tx
            .send(result.landings[0].measurement_deferred)
            .unwrap();
    });
    let fast = done_rx.recv_timeout(Duration::from_millis(1600));
    let returned_while_blocked = fast.is_ok();
    unprobed.resume();
    let deferred = fast.unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    worker.join().unwrap();
    assert!(
        returned_while_blocked,
        "an unprobed owner became synchronous after a same-origin resize"
    );
    assert!(
        deferred,
        "a same-origin resize drained an unprobed owner's deferral"
    );
    for _ in 0..3 {
        let result = leopardwm_platform_win32::apply_placements(
            std::slice::from_ref(&placement),
            &state.platform_config,
            None,
            false,
        )
        .unwrap();
        if !result.landings[0].measurement_deferred {
            assert_eq!(
                leopardwm_platform_win32::get_window_visible_rect(window_id),
                Some(placement.rect)
            );
            return;
        }
    }
    panic!("unprobed owner did not drain after responding");
}

#[test]
fn test_owner_wait_inactive_workspace_parking_does_not_block_display_change() {
    let _serial = REAL_WINDOW_STYLE_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let blocked = PlacementOwner::spawn();
    let window_id = blocked.window_id;
    let (done_tx, done_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut state = placement_state(&[]);
        state.injected_native_offscreen_enabled = true;
        let inactive = state.workspaces[&1][0].clone();
        state.workspaces.get_mut(&1).unwrap().push(inactive);
        state.workspaces.get_mut(&1).unwrap()[1]
            .insert_window(window_id, Some(320))
            .unwrap();
        state
            .injected_window_info
            .insert(window_id, make_test_window_info(window_id));
        state.handle_window_event(WindowEvent::DisplayChange);
        done_tx.send(!state.paused).unwrap();
    });
    let fast = done_rx.recv_timeout(Duration::from_millis(400));
    let returned_while_blocked = fast.is_ok();
    blocked.resume();
    let active = fast.unwrap_or_else(|_| done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    worker.join().unwrap();
    assert!(
        returned_while_blocked,
        "display reconciliation synchronously parked an inactive-workspace window"
    );
    assert!(active);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
        &leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap(),
    ) && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(leopardwm_platform_win32::is_move_offscreen_sentinel_rect(
        &leopardwm_platform_win32::get_window_chrome_rect(window_id).unwrap()
    ));
    let placement = fixture_placement(window_id, Visibility::Visible);
    let state = placement_state(&[]);
    for _ in 0..3 {
        leopardwm_platform_win32::apply_placements(
            std::slice::from_ref(&placement),
            &state.platform_config,
            None,
            false,
        )
        .unwrap();
    }
    assert_eq!(
        leopardwm_platform_win32::get_window_visible_rect(window_id),
        Some(placement.rect),
        "queued inactive parking overtook return-to-visible placement"
    );
}
