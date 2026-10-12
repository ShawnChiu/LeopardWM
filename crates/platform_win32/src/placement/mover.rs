use super::{cached_placement_owner_answer, DeferEntry};
use crate::window_id_to_hwnd;
use leopardwm_core_layout::WindowId;
use std::collections::HashMap;
use std::sync::{Condvar, LazyLock, Mutex};
use windows::Win32::UI::WindowsAndMessaging::{
    SetWindowPos, SET_WINDOW_POS_FLAGS, SWP_ASYNCWINDOWPOS,
};

#[derive(Clone, Copy)]
struct Target {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    flags: SET_WINDOW_POS_FLAGS,
}

struct Slot {
    lifetime: u64,
    pending: Option<Target>,
}

#[derive(Default)]
struct State {
    slots: HashMap<WindowId, Slot>,
    next_lifetime: u64,
    in_flight: Option<(WindowId, u64)>,
    started: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));
static WAKE: Condvar = Condvar::new();

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(crate::recover_poisoned_mutex)
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
    if let Some(slot) = state.slots.get_mut(&window_id) {
        slot.pending = None;
    }
    state.in_flight.is_some_and(|(id, _)| id == window_id)
}

pub(super) fn forget(window_id: WindowId) {
    lock().slots.remove(&window_id);
}

pub(super) fn submit(entry: &DeferEntry) {
    let mut state = lock();
    let Some(slot) = state.slots.get_mut(&entry.window_id) else {
        return;
    };
    slot.pending = Some(Target {
        x: entry.x,
        y: entry.y,
        w: entry.w,
        h: entry.h,
        flags: entry.flags & !SWP_ASYNCWINDOWPOS,
    });
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
                if let Some((&id, slot)) = state
                    .slots
                    .iter_mut()
                    .find(|(_, slot)| slot.pending.is_some())
                {
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
            let current_lifetime = lock()
                .slots
                .get(&window_id)
                .is_some_and(|slot| slot.lifetime == lifetime);
            if answered && current_lifetime {
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
            }
        }
        lock().in_flight = None;
    }
}
