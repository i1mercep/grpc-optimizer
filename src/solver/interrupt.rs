//! Cooperative interruption through HiGHS callbacks absent from the safe wrapper API.
//!
//! The native model owns the callback registration; its state must outlive it.

use std::{
    ffi::{c_char, c_int, c_void},
    ops::Deref,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

use highs::{Model, SolvedModel};
use highs_sys::{
    Highs_setCallback, Highs_startCallback, HighsCallbackDataIn, HighsCallbackDataOut,
    kHighsCallbackIpmInterrupt, kHighsCallbackSimplexInterrupt, kHighsStatusOk,
};
use tokio_util::sync::CancellationToken;

use crate::model::{Error, Result};

/// State kept at a stable address while HiGHS can invoke the callback.
struct Callback {
    cancel: CancellationToken,
    deadline: Option<Instant>,
    panicked: AtomicBool,
    #[cfg(test)]
    test_control: Option<std::sync::Arc<TestControl>>,
}

/// Keeps callback state alive through solution reads and native model destruction.
pub(super) struct Solved {
    // Fields drop in declaration order: destroy HiGHS before freeing the callback
    // state. `into_model` detaches that callback, then releases `native`.
    native: SolvedModel,
    _callback: Box<Callback>,
}

impl Solved {
    /// Detaches the interrupt callback so the model can outlive this state.
    ///
    /// Stopping a callback leaves HiGHS holding the user pointer. Clearing it is
    /// what makes the pointer safe to free.
    pub(super) fn into_model(self) -> Result<Model> {
        let mut native = Model::from(self.native);
        // SAFETY: the model is exclusively owned and this callback state is still
        // alive. `Highs_setCallback` drops HiGHS's copy of that pointer before return.
        let status = unsafe { Highs_setCallback(native.as_mut_ptr(), None, std::ptr::null_mut()) };
        check_status(status, "clear interrupt callback")?;
        Ok(native)
    }
}

impl Deref for Solved {
    type Target = SolvedModel;

    fn deref(&self) -> &Self::Target {
        &self.native
    }
}

/// Registers simplex/IPM interruption and solves while preserving callback lifetimes.
/// Work between callbacks may delay cancellation; budget checks also run around the solve.
pub(super) fn solve(
    native: Model,
    cancel: &CancellationToken,
    deadline: Option<Instant>,
    #[cfg(test)] test_control: Option<std::sync::Arc<TestControl>>,
) -> Result<Solved> {
    let callback = Box::new(Callback {
        cancel: cancel.clone(),
        deadline,
        panicked: AtomicBool::new(false),
        #[cfg(test)]
        test_control,
    });
    // Move the native owner into this scope so every early return destroys it
    // while `callback` is still alive, including partial registration failures.
    let native = {
        let callback = &*callback;
        (move || {
            let mut native = native;
            #[cfg(test)]
            if let Some(control) = &callback.test_control {
                control.configure(&mut native, cancel)?;
            }
            // SAFETY: the model is exclusively owned by this worker. The boxed
            // callback has a stable address and outlives the native owner on all
            // paths. HiGHS only reads shared state through the callback.
            let status = unsafe {
                Highs_setCallback(
                    native.as_mut_ptr(),
                    Some(interrupt),
                    std::ptr::from_ref(callback).cast_mut().cast(),
                )
            };
            check_status(status, "register interrupt callback")?;
            for kind in [kHighsCallbackSimplexInterrupt, kHighsCallbackIpmInterrupt] {
                // SAFETY: this is the same live model, and both callback types
                // are constants supplied by the linked HiGHS bindings.
                let status = unsafe { Highs_startCallback(native.as_mut_ptr(), kind) };
                check_status(status, "enable interrupt callback")?;
            }
            super::check_budget(deadline, cancel)?;
            let result = native.try_solve();
            #[cfg(test)]
            if let Some(control) = &callback.test_control {
                *control.status.lock().unwrap() = result.as_ref().ok().map(|m| m.status());
            }
            if callback.panicked.load(Ordering::Relaxed) {
                return Err(Error::Internal("HiGHS interrupt callback panicked".into()));
            }
            // A deadline may expire in presolve or near normal completion,
            // without HiGHS ever reaching an interrupt callback.
            super::check_budget(deadline, cancel)?;
            result.map_err(|s| Error::Internal(format!("HiGHS solve failed: {s:?}")))
        })()
    }?;
    Ok(Solved {
        native,
        _callback: callback,
    })
}

fn check_status(status: highs_sys::HighsInt, operation: &str) -> Result<()> {
    if status == kHighsStatusOk {
        Ok(())
    } else {
        Err(Error::Internal(format!(
            "HiGHS {operation}: status {status}"
        )))
    }
}

/// Requests interruption on cancellation, expiry, or panic without unwinding into C.
///
/// # Safety
/// Non-null `user_data` must point to a live [`Callback`]. Non-null `input` must be
/// writable HiGHS callback input for this invocation. Null pointers are ignored.
unsafe extern "C" fn interrupt(
    _kind: c_int,
    _message: *const c_char,
    _output: *const HighsCallbackDataOut,
    input: *mut HighsCallbackDataIn,
    user_data: *mut c_void,
) {
    if input.is_null() || user_data.is_null() {
        return;
    }
    // SAFETY: registration supplies a boxed Callback alive for the complete
    // native lifetime. HiGHS supplies writable callback input for this call.
    let callback = unsafe { &*user_data.cast::<Callback>() };
    let stop = catch_unwind(AssertUnwindSafe(|| {
        #[cfg(test)]
        if let Some(control) = &callback.test_control {
            control.on_callback();
        }
        callback.cancel.is_cancelled() || callback.deadline.is_some_and(|d| Instant::now() >= d)
    }))
    .unwrap_or_else(|_| {
        callback.panicked.store(true, Ordering::Relaxed);
        true
    });
    if stop {
        // SAFETY: checked non-null above; only HiGHS owns this callback input.
        unsafe { (*input).user_interrupt = 1 };
    }
}

/// A per-engine gate used only by tests to observe and pause the first native
/// callback. No global hooks or solver options are exposed in production.
#[cfg(test)]
pub(crate) struct TestControl {
    pub request_id: String,
    solver: &'static str,
    started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<CancellationToken>>>,
    resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    cancel: std::sync::Mutex<Option<CancellationToken>>,
    pub status: std::sync::Mutex<Option<highs::HighsModelStatus>>,
}

#[cfg(test)]
impl TestControl {
    /// Creates a gate, a first-callback notification, and a sender that releases the gate.
    pub fn new(
        request_id: &str,
        solver: &'static str,
    ) -> (
        std::sync::Arc<Self>,
        tokio::sync::oneshot::Receiver<CancellationToken>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, resume) = std::sync::mpsc::channel();
        (
            std::sync::Arc::new(Self {
                request_id: request_id.into(),
                solver,
                started: std::sync::Mutex::new(Some(started)),
                resume: std::sync::Mutex::new(resume),
                cancel: std::sync::Mutex::new(None),
                status: std::sync::Mutex::new(None),
            }),
            entered,
            release,
        )
    }

    /// Disables presolve and selects an algorithm so tests reach its interrupt callback.
    fn configure(&self, native: &mut Model, cancel: &CancellationToken) -> Result<()> {
        *self.cancel.lock().unwrap() = Some(cancel.clone());
        native
            .try_set_option("presolve", "off")
            .map_err(|s| Error::Internal(format!("test presolve: {s:?}")))?;
        native
            .try_set_option("solver", self.solver)
            .map_err(|s| Error::Internal(format!("test solver: {s:?}")))?;
        Ok(())
    }

    /// Pauses only the first callback so a test can cancel or expire an active solve.
    fn on_callback(&self) {
        let started = self.started.lock().unwrap().take();
        if let Some(started) = started {
            let _ = started.send(self.cancel.lock().unwrap().as_ref().unwrap().clone());
            // Bound a failed test's wait and unblock on sender drop as well.
            let _ = self
                .resume
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(10));
        }
    }
}
