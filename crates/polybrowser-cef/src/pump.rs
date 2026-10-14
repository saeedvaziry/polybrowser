//! Runs Chromium's work on the application's main run loop.
//!
//! Chromium is started with an "external message pump": instead of owning
//! the event loop, it asks to be called back after a delay, from any thread.
//! Those requests are forwarded to the main dispatch queue, which every
//! AppKit event loop services whatever toolkit runs it, so the host
//! application does not have to cooperate. The scheduling follows CEF's
//! reference implementation, `main_message_loop_external_pump.cc`.

use std::{
    cell::Cell,
    ffi::c_void,
    sync::atomic::{AtomicBool, Ordering},
};

/// What Chromium passes when it has nothing scheduled.
const NO_DELAY_REQUESTED: i64 = i32::MAX as i64;
/// Longest gap between two rounds of work, in milliseconds (30 per second).
const MAX_DELAY_MS: i64 = 1000 / 30;

extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn dispatch_after_f(
        when: u64,
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn dispatch_time(when: u64, delta_nanoseconds: i64) -> u64;
}

static RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct Pump {
    working: Cell<bool>,
    reentered: Cell<bool>,
    timer_pending: Cell<bool>,
    /// Timers cannot be cancelled, so each carries the generation it was set
    /// in and is ignored if the generation has moved on.
    timer_generation: Cell<usize>,
}

thread_local! {
    static PUMP: Pump = Pump::default();
}

pub(crate) fn start() {
    RUNNING.store(true, Ordering::Release);
    schedule(0);
}

pub(crate) fn stop() {
    RUNNING.store(false, Ordering::Release);
}

/// Asks for a round of work in `delay_ms`. May be called on any thread.
pub(crate) fn schedule(delay_ms: i64) {
    // The delay travels as the context pointer itself.
    let context = delay_ms.clamp(0, NO_DELAY_REQUESTED) as usize as *mut c_void;
    // SAFETY: the main queue lives for the whole process and the context is
    // a plain number, never dereferenced.
    unsafe { dispatch_async_f(&raw const _dispatch_main_q, context, on_scheduled) };
}

/// Does a round of work right away. Main thread only; used while waiting for
/// Chromium synchronously.
pub(crate) fn work_now() {
    let _ = PUMP.try_with(Pump::perform_work);
}

extern "C" fn on_scheduled(context: *mut c_void) {
    let delay_ms = context as usize as i64;
    let _ = PUMP.try_with(|pump| pump.on_scheduled(delay_ms));
}

extern "C" fn on_timer(context: *mut c_void) {
    let generation = context as usize;
    let _ = PUMP.try_with(|pump| {
        if pump.timer_pending.get() && pump.timer_generation.get() == generation {
            pump.kill_timer();
            pump.do_work();
        }
    });
}

impl Pump {
    fn on_scheduled(&self, delay_ms: i64) {
        if !RUNNING.load(Ordering::Acquire) {
            return;
        }
        if delay_ms == NO_DELAY_REQUESTED && self.timer_pending.get() {
            return;
        }
        self.kill_timer();
        if delay_ms <= 0 {
            self.do_work();
        } else {
            self.set_timer(delay_ms.min(MAX_DELAY_MS));
        }
    }

    fn do_work(&self) {
        if self.perform_work() {
            // Work was requested while we were working; run again soon.
            schedule(0);
        } else if !self.timer_pending.get() {
            schedule(NO_DELAY_REQUESTED);
        }
    }

    /// Returns whether another round was asked for while this one ran.
    fn perform_work(&self) -> bool {
        if !RUNNING.load(Ordering::Acquire) {
            return false;
        }
        if self.working.get() {
            // Chromium is already working further up this stack (a menu or a
            // drag runs a nested loop). Come back when it has returned.
            self.reentered.set(true);
            return false;
        }
        self.reentered.set(false);
        self.working.set(true);
        cef::do_message_loop_work();
        self.working.set(false);
        self.reentered.get()
    }

    fn set_timer(&self, delay_ms: i64) {
        let generation = self.timer_generation.get().wrapping_add(1);
        self.timer_generation.set(generation);
        self.timer_pending.set(true);
        // SAFETY: as in `schedule`.
        unsafe {
            dispatch_after_f(
                dispatch_time(0, delay_ms * 1_000_000),
                &raw const _dispatch_main_q,
                generation as *mut c_void,
                on_timer,
            );
        }
    }

    fn kill_timer(&self) {
        self.timer_pending.set(false);
        self.timer_generation
            .set(self.timer_generation.get().wrapping_add(1));
    }
}
