//! Native models kept by a session, so each job re-solves from the previous basis.

use std::sync::{Mutex, MutexGuard};

use highs::Model;
use highs_sys::{
    Highs_changeColsBoundsByRange, Highs_changeColsCostByRange, Highs_changeRowsBoundsByRange,
    Highs_getObjectiveSense, Highs_zeroAllClocks, HighsInt, OBJECTIVE_SENSE_MAXIMIZE,
    OBJECTIVE_SENSE_MINIMIZE, kHighsStatusOk, kHighsStatusWarning,
};

use crate::{
    model::{Error, Result},
    pb::{LinearProgram, ObjectiveSense},
};

/// A session's reusable model; empty while a job uses it or after a failed job.
pub(crate) type WarmModel = Mutex<Option<Warm>>;

pub(crate) struct Warm(pub(super) Model);

// SAFETY: `highs::Model` is `!Send` only because it holds a raw pointer. A HiGHS
// instance has no thread affinity, and this wrapper is moved between workers, never shared.
unsafe impl Send for Warm {}

/// Puts a prepared model back if solve never takes ownership.
///
/// Only an empty slot is filled, so a sibling job's stored model is left alone.
pub(super) struct Guard<'a> {
    slot: Option<&'a WarmModel>,
    model: Option<Model>,
}

impl<'a> Guard<'a> {
    pub(super) fn hold(slot: Option<&'a WarmModel>, model: Model) -> Self {
        Self {
            slot,
            model: Some(model),
        }
    }

    pub(super) fn model_mut(&mut self) -> &mut Model {
        self.model.as_mut().expect("warm guard holds a model")
    }

    pub(super) fn take(&mut self) -> Model {
        self.model.take().expect("warm guard holds a model")
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if let (Some(slot), Some(model)) = (self.slot, self.model.take()) {
            fill_if_empty(slot, model);
        }
    }
}

/// Removes the model, if another job has not already taken it.
pub(super) fn take(slot: &WarmModel) -> Option<Warm> {
    lock(slot).take()
}

/// Stores a solved model, replacing whatever a sibling job left behind.
pub(super) fn store(slot: &WarmModel, model: Model) {
    *lock(slot) = Some(Warm(model));
}

fn fill_if_empty(slot: &WarmModel, model: Model) {
    let mut guard = lock(slot);
    if guard.is_none() {
        *guard = Some(Warm(model));
    }
}

/// Recovers a poisoned slot. The lock only covers the `Option` swap, so the model is intact.
fn lock(slot: &WarmModel) -> MutexGuard<'_, Option<Warm>> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Warm {
    /// True when costs and bounds can be copied without changing the matrix or sense.
    pub(super) fn compatible(&self, input: &LinearProgram) -> bool {
        self.0.num_cols() == input.variables.len()
            && self.0.num_rows() == input.constraints.len()
            && objective_sense(&self.0) == Some(expected_sense(input))
    }

    /// Copies costs and bounds from `input`. The caller must check [`Self::compatible`].
    pub(super) fn refresh(self, input: &LinearProgram) -> Result<Model> {
        if !self.compatible(input) {
            return Err(Error::Internal(
                "warm model structure does not match the request".into(),
            ));
        }
        let mut native = self.0;
        let costs: Vec<f64> = input.variables.iter().map(|v| v.objective).collect();
        let (col_lower, col_upper) = bounds(input.variables.iter().map(|v| (v.lower, v.upper)));
        let (row_lower, row_upper) = bounds(input.constraints.iter().map(|r| (r.lower, r.upper)));
        let col_last = last_index(costs.len())?;
        let row_last = if row_lower.is_empty() {
            None
        } else {
            Some(last_index(row_lower.len())?)
        };
        let ptr = native.as_mut_ptr();
        // SAFETY: the model is exclusively owned. `compatible` proved each array has
        // one entry per column or row, and `last_index` fits in `HighsInt`.
        let statuses = unsafe {
            [
                // HiGHS checks `time_limit` against run time accumulated over all solves.
                Highs_zeroAllClocks(ptr),
                Highs_changeColsCostByRange(ptr, 0, col_last, costs.as_ptr()),
                Highs_changeColsBoundsByRange(
                    ptr,
                    0,
                    col_last,
                    col_lower.as_ptr(),
                    col_upper.as_ptr(),
                ),
                if let Some(row_last) = row_last {
                    Highs_changeRowsBoundsByRange(
                        ptr,
                        0,
                        row_last,
                        row_lower.as_ptr(),
                        row_upper.as_ptr(),
                    )
                } else {
                    kHighsStatusOk
                },
            ]
        };
        // Warnings only report huge finite bounds treated as infinite, as when building.
        if statuses
            .iter()
            .any(|status| *status != kHighsStatusOk && *status != kHighsStatusWarning)
        {
            return Err(Error::Internal("HiGHS rejected a warm model update".into()));
        }
        Ok(native)
    }
}

fn objective_sense(model: &Model) -> Option<HighsInt> {
    let mut sense = 0;
    // SAFETY: the model is exclusively owned by this worker. HiGHS writes one integer.
    let status = unsafe { Highs_getObjectiveSense(model.as_ptr(), &mut sense) };
    (status == kHighsStatusOk).then_some(sense)
}

fn expected_sense(input: &LinearProgram) -> HighsInt {
    if input.sense == ObjectiveSense::Maximize as i32 {
        OBJECTIVE_SENSE_MAXIMIZE
    } else {
        OBJECTIVE_SENSE_MINIMIZE
    }
}

fn bounds(items: impl Iterator<Item = (Option<f64>, Option<f64>)>) -> (Vec<f64>, Vec<f64>) {
    items
        .map(|(lower, upper)| {
            (
                lower.unwrap_or(f64::NEG_INFINITY),
                upper.unwrap_or(f64::INFINITY),
            )
        })
        .unzip()
}

fn last_index(len: usize) -> Result<HighsInt> {
    let len = HighsInt::try_from(len)
        .map_err(|_| Error::Internal("model dimension exceeds HiGHS integer range".into()))?;
    Ok(len - 1)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        config::Config,
        pb::SolveStatus,
        samples::{Family, generate},
        solver::{TestControl, solve, solve_inner},
    };

    #[tokio::test]
    async fn warm_models_match_fresh_solves_without_accumulating_solver_time() {
        let base = generate(Family::Production, 100, 42).unwrap();
        let warm = Arc::new(WarmModel::default());
        let (control, entered, release) = TestControl::new(&base.request_id, "simplex");
        let (request, slot) = (base.clone(), warm.clone());
        let paused = tokio::task::spawn_blocking(move || {
            let cancel = CancellationToken::new();
            solve_inner(
                request,
                &Config::default(),
                None,
                &cancel,
                Some(&slot),
                Some(control),
            )
        });
        tokio::time::timeout(Duration::from_secs(5), entered)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        release.send(()).unwrap();
        paused.await.unwrap().unwrap();
        // New profits force simplex iterations, where HiGHS checks its time limit.
        let mut repriced = base.clone();
        repriced.time_limit_seconds = Some(0.2);
        for (i, var) in repriced
            .model
            .as_mut()
            .unwrap()
            .variables
            .iter_mut()
            .enumerate()
        {
            var.objective = 5.0 + i as f64;
        }
        for request in [repriced, base] {
            let cancel = CancellationToken::new();
            let fresh = solve(request.clone(), &Config::default(), None, &cancel).unwrap();
            let reused = solve_inner(
                request,
                &Config::default(),
                None,
                &cancel,
                Some(&warm),
                None,
            )
            .unwrap();
            assert!(warm.lock().unwrap().is_some());
            assert_eq!(reused.status, SolveStatus::Optimal as i32);
            let objective = |r: &crate::pb::SolveResult| r.solution.as_ref().unwrap().objective;
            assert!((objective(&reused) - objective(&fresh)).abs() < 1e-6);
        }
    }

    #[test]
    fn mismatched_structure_rebuilds_instead_of_updating_a_prefix() {
        let small = generate(Family::Production, 2, 1).unwrap();
        let large = generate(Family::Production, 5, 1).unwrap();
        let warm = WarmModel::default();
        let cancel = CancellationToken::new();
        solve_inner(small, &Config::default(), None, &cancel, Some(&warm), None).unwrap();
        let reused = solve_inner(
            large.clone(),
            &Config::default(),
            None,
            &cancel,
            Some(&warm),
            None,
        )
        .unwrap();
        let fresh = solve(large, &Config::default(), None, &cancel).unwrap();
        assert_eq!(reused.status, SolveStatus::Optimal as i32);
        let objective = |r: &crate::pb::SolveResult| r.solution.as_ref().unwrap().objective;
        assert!((objective(&reused) - objective(&fresh)).abs() < 1e-6);
        let stored = warm
            .lock()
            .unwrap()
            .take()
            .expect("rebuilt model is stored");
        assert_eq!(
            stored.0.num_cols(),
            reused.solution.unwrap().variable_values.len()
        );
    }
}
