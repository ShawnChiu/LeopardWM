use super::{cached_placement_owner_answer, placement_owner_answers, DeferEntry};
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
    generation: u64,
}

struct Slot {
    lifetime: u64,
    pending: Option<Target>,
    landing: Option<Target>,
    generation: u64,
    cancelled: u64,
}

#[derive(Default)]
struct State {
    slots: HashMap<WindowId, Slot>,
    ready: VecDeque<WindowId>,
    next_generation: u64,
    in_flight: Option<(WindowId, u64)>,
    started: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));
static WAKE: Condvar = Condvar::new();

fn lock() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(crate::recover_poisoned_mutex)
}

pub(super) fn generations() -> HashMap<WindowId, u64> {
    lock()
        .slots
        .iter()
        .map(|(&id, slot)| (id, slot.generation))
        .collect()
}

pub(super) fn mark(window_id: WindowId) {
    let mut state = lock();
    if state.slots.contains_key(&window_id) {
        return;
    }
    state.next_generation += 1;
    let lifetime = state.next_generation;
    state.slots.insert(
        window_id,
        Slot {
            lifetime,
            pending: None,
            landing: None,
            generation: lifetime,
            cancelled: 0,
        },
    );
}

pub(super) fn is_marked(window_id: WindowId) -> bool {
    lock().slots.contains_key(&window_id)
}

pub(super) fn in_flight(window_id: WindowId) -> bool {
    lock().in_flight.is_some_and(|(id, _)| id == window_id)
}

pub(super) fn seal(entry: &DeferEntry) -> bool {
    let window_id = entry.window_id;
    let mut state = lock();
    if !state.slots.contains_key(&window_id) {
        return false;
    }
    state.next_generation += 1;
    let generation = state.next_generation;
    let in_flight = state.in_flight.is_some_and(|(id, _)| id == window_id);
    if let Some(slot) = state.slots.get_mut(&window_id) {
        slot.generation = generation;
        slot.pending = None;
        slot.landing = in_flight.then_some(Target {
            x: entry.x,
            y: entry.y,
            w: entry.w,
            h: entry.h,
            flags: entry.flags & !SWP_ASYNCWINDOWPOS,
            generation,
        });
    }
    state.ready.retain(|&id| id != window_id);
    in_flight
}

pub(super) fn forget(window_id: WindowId) {
    let mut state = lock();
    state.slots.remove(&window_id);
    state.ready.retain(|&id| id != window_id);
}

pub(super) fn cancel(window_ids: Option<&[WindowId]>) {
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut state = lock();
    let affected = |id: WindowId| window_ids.is_none_or(|ids| ids.contains(&id));
    state.ready.retain(|&id| !affected(id));
    state.next_generation += 1;
    let generation = state.next_generation;
    for (&id, slot) in &mut state.slots {
        if affected(id) {
            slot.generation = generation;
            slot.cancelled = slot.generation;
            slot.pending = None;
            slot.landing = None;
        }
    }
    while state.in_flight.is_some_and(|(id, _)| affected(id)) {
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

pub(super) fn submit(entry: &DeferEntry, generation: Option<u64>) {
    let mut state = lock();
    let Some(slot) = state.slots.get_mut(&entry.window_id) else {
        return;
    };
    if Some(slot.generation) != generation {
        return;
    }
    let generation = slot.generation;
    let enqueue = slot.pending.is_none();
    slot.pending = Some(Target {
        x: entry.x,
        y: entry.y,
        w: entry.w,
        h: entry.h,
        flags: entry.flags & !SWP_ASYNCWINDOWPOS,
        generation,
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
        let mut before = None;
        let mut moved = false;
        if let Ok(hwnd) = window_id_to_hwnd(window_id) {
            let answered = cached_placement_owner_answer(hwnd);
            let current = {
                let state = lock();
                state.slots.get(&window_id).is_some_and(|slot| {
                    slot.lifetime == lifetime && slot.generation == target.generation
                })
            };
            if answered && current {
                let mut rect = RECT::default();
                before = unsafe { GetWindowRect(hwnd, &mut rect) }
                    .ok()
                    .map(|()| rect);
                moved = true;
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
        finish(window_id, lifetime, target.generation, before, moved);
    }
}

fn finish(
    window_id: WindowId,
    lifetime: u64,
    mut generation: u64,
    before: Option<RECT>,
    mut moved: bool,
) {
    loop {
        let correction = {
            let mut state = lock();
            let correction = state
                .slots
                .get(&window_id)
                .filter(|slot| slot.lifetime == lifetime)
                .and_then(|slot| {
                    if let Some(landing) =
                        slot.landing.filter(|target| target.generation > generation)
                    {
                        Some((landing.generation, Some(landing)))
                    } else if moved
                        && slot.cancelled > generation
                        && slot.generation == slot.cancelled
                    {
                        Some((slot.generation, None))
                    } else {
                        None
                    }
                });
            if correction.is_none() {
                state.in_flight = None;
                WAKE.notify_all();
                return;
            }
            correction.unwrap()
        };
        let (next_generation, landing) = correction;
        let Ok(hwnd) = window_id_to_hwnd(window_id) else {
            generation = next_generation;
            continue;
        };
        let answered = placement_owner_answers(hwnd, 50);
        let current = lock()
            .slots
            .get(&window_id)
            .is_some_and(|slot| slot.lifetime == lifetime && slot.generation == next_generation);
        if !current {
            continue;
        }
        if answered {
            if let Some(target) = landing {
                moved = true;
                if let Err(error) = unsafe {
                    SetWindowPos(
                        hwnd,
                        None,
                        target.x,
                        target.y,
                        target.w,
                        target.h,
                        target.flags,
                    )
                } {
                    tracing::warn!(window_id, %error, "Could not complete deferred placement landing");
                }
            } else {
                crate::visibility::recover_after_placement_mover(window_id, before);
            }
        } else {
            tracing::warn!(
                window_id,
                "Skipping late placement correction: owner did not answer"
            );
        }
        generation = next_generation;
    }
}
