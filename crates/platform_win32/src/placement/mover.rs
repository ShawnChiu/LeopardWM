use super::{cached_placement_owner_answer, DeferEntry};
use crate::window_id_to_hwnd;
use leopardwm_core_layout::WindowId;
use std::collections::{HashMap, VecDeque};
use std::sync::{Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, SetWindowPos, SET_WINDOW_POS_FLAGS, SWP_ASYNCWINDOWPOS,
};

#[derive(Clone, Copy)]
struct Target {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    flags: SET_WINDOW_POS_FLAGS,
    epoch: u64,
}

struct Slot {
    lifetime: u64,
    pending: Option<Target>,
}

#[derive(Default)]
struct State {
    slots: HashMap<WindowId, Slot>,
    ready: VecDeque<WindowId>,
    next_lifetime: u64,
    epoch: u64,
    in_flight: Option<(WindowId, u64)>,
    started: bool,
    stopped: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));
static WAKE: Condvar = Condvar::new();

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(crate::recover_poisoned_mutex)
}

pub(super) fn epoch() -> u64 {
    lock().epoch
}

pub(super) fn mark(window_id: WindowId) {
    let mut state = lock();
    if state.slots.contains_key(&window_id) {
        return;
    }
    state.next_lifetime += 1;
    let lifetime = state.next_lifetime;
    state.slots.insert(
        window_id,
        Slot {
            lifetime,
            pending: None,
        },
    );
}

pub(super) fn is_marked(window_id: WindowId) -> bool {
    lock().slots.contains_key(&window_id)
}

pub(super) fn in_flight(window_id: WindowId) -> bool {
    lock().in_flight.is_some_and(|(id, _)| id == window_id)
}

pub(super) fn seal(window_id: WindowId) -> bool {
    let mut state = lock();
    state.epoch += 1;
    if let Some(slot) = state.slots.get_mut(&window_id) {
        slot.pending = None;
    }
    state.ready.retain(|&id| id != window_id);
    state.in_flight.is_some_and(|(id, _)| id == window_id)
}

pub(super) fn forget(window_id: WindowId) {
    let mut state = lock();
    state.slots.remove(&window_id);
    state.ready.retain(|&id| id != window_id);
}

pub(super) fn stop() {
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut state = lock();
    state.stopped = true;
    state.epoch += 1;
    state.ready.clear();
    for slot in state.slots.values_mut() {
        slot.pending = None;
    }
    WAKE.notify_all();
    while state.in_flight.is_some() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        state = WAKE
            .wait_timeout(state, remaining)
            .unwrap_or_else(|error| error.into_inner())
            .0;
    }
}

pub(super) fn submit(entry: &DeferEntry, epoch: u64) {
    let mut state = lock();
    if state.stopped || state.epoch != epoch {
        return;
    }
    let Some(slot) = state.slots.get_mut(&entry.window_id) else {
        return;
    };
    let enqueue = slot.pending.is_none();
    slot.pending = Some(Target {
        x: entry.x,
        y: entry.y,
        w: entry.w,
        h: entry.h,
        flags: entry.flags & !SWP_ASYNCWINDOWPOS,
        epoch,
    });
    if enqueue {
        state.ready.push_back(entry.window_id);
    }
    if !state.started {
        match std::thread::Builder::new()
            .name("placement-mover".into())
            .spawn(run)
        {
            Ok(_) => state.started = true,
            Err(error) => tracing::warn!(%error, "Could not start placement mover"),
        }
    }
    WAKE.notify_one();
}

fn run() {
    loop {
        let (window_id, lifetime, target) = {
            let mut state = lock();
            loop {
                if state.stopped {
                    return;
                }
                if let Some(id) = state.ready.pop_front() {
                    let slot = state.slots.get_mut(&id).unwrap();
                    let target = slot.pending.take().unwrap();
                    let lifetime = slot.lifetime;
                    state.in_flight = Some((id, lifetime));
                    break (id, lifetime, target);
                }
                state = WAKE
                    .wait(state)
                    .unwrap_or_else(crate::recover_poisoned_mutex);
            }
        };
        if let Ok(hwnd) = window_id_to_hwnd(window_id) {
            let answered = cached_placement_owner_answer(hwnd);
            let current = {
                let state = lock();
                !state.stopped
                    && state.epoch == target.epoch
                    && state
                        .slots
                        .get(&window_id)
                        .is_some_and(|slot| slot.lifetime == lifetime)
            };
            if answered && current {
                let mut before = RECT::default();
                let before = unsafe { GetWindowRect(hwnd, &mut before) }
                    .ok()
                    .map(|()| before);
                // A probe-to-hang race can stall this one mover, never the animation worker.
                let result = unsafe {
                    SetWindowPos(
                        hwnd,
                        None,
                        target.x,
                        target.y,
                        target.w,
                        target.h,
                        target.flags,
                    )
                };
                if let Err(error) = result {
                    tracing::debug!(window_id, %error, "Placement mover could not apply frame");
                }
                let recover = {
                    let state = lock();
                    state.stopped
                        && state
                            .slots
                            .get(&window_id)
                            .is_some_and(|slot| slot.lifetime == lifetime)
                };
                if recover {
                    // A call that outlives stop's bounded wait must finish with visibility recovery.
                    crate::visibility::recover_after_placement_mover(window_id, before);
                }
            }
        }
        lock().in_flight = None;
        WAKE.notify_all();
    }
}
