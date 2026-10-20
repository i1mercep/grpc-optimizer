use std::{ops::Bound, time::Instant};

use highs::{HighsModelStatus, HighsSolutionStatus, RowProblem, Sense};
use tokio_util::sync::CancellationToken;

use crate::{
    config::Config,
    model::{self, Error, Result},
    pb::{DualSolution, ObjectiveSense, Solution, SolveRequest, SolveResult, SolveStatus, Timings},
};

mod interrupt;

#[cfg(test)]
pub(crate) use interrupt::TestControl;

/// Synchronous entry point. Call only from a blocking worker (or the direct benchmark).
pub fn solve(
    request: SolveRequest,
    config: &Config,
    deadline: Option<Instant>,
    cancel: &CancellationToken,
) -> Result<SolveResult> {
    solve_inner(
        request,
        config,
        deadline,
        cancel,
        #[cfg(test)]
        None,
    )
}

pub(crate) fn solve_inner(
    request: SolveRequest,
    config: &Config,
    deadline: Option<Instant>,
    cancel: &CancellationToken,
    #[cfg(test)] test_control: Option<std::sync::Arc<TestControl>>,
) -> Result<SolveResult> {
    check_budget(deadline, cancel)?;
    let start = Instant::now();
    model::validate(&request, config)?;
    let validation_seconds = start.elapsed().as_secs_f64();
    let build_start = Instant::now();
    let input = request.model.as_ref().expect("validated model");
    let mut problem = RowProblem::new();
    let columns: Vec<_> = input
        .variables
        .iter()
        .map(|v| problem.add_column(v.objective, range(v.lower, v.upper)))
        .collect();
    for row in &input.constraints {
        problem.add_row(
            range(row.lower, row.upper),
            row.indices
                .iter()
                .zip(&row.coefficients)
                .map(|(&index, &value)| (columns[index as usize], value)),
        );
    }
    let sense = if input.sense == ObjectiveSense::Maximize as i32 {
        Sense::Maximise
    } else {
        Sense::Minimise
    };
    let mut native = problem
        .try_optimise(sense)
        .map_err(|s| Error::Invalid(format!("HiGHS rejected model: {s:?}")))?;
    native.make_quiet();
    native
        .try_set_option("threads", config.solver_threads as i32)
        .map_err(|e| Error::Internal(format!("HiGHS threads: {e:?}")))?;
    native
        .try_set_option(
            "parallel",
            if config.solver_threads == 1 {
                "off"
            } else {
                "on"
            },
        )
        .map_err(|e| Error::Internal(format!("HiGHS parallel: {e:?}")))?;
    check_budget(deadline, cancel)?;
    let mut budget = request
        .time_limit_seconds
        .unwrap_or(config.default_time_limit_seconds)
        .min(config.max_time_limit_seconds);
    if let Some(deadline) = deadline {
        budget = budget.min(
            deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64(),
        );
    }
    if budget <= 0.0 {
        return Err(Error::Deadline);
    }
    native
        .try_set_option("time_limit", budget)
        .map_err(|e| Error::Internal(format!("HiGHS time limit: {e:?}")))?;
    let build_seconds = build_start.elapsed().as_secs_f64();
    let solve_start = Instant::now();
    let solved = interrupt::solve(
        native,
        cancel,
        deadline,
        #[cfg(test)]
        test_control,
    )?;
    let solve_seconds = solve_start.elapsed().as_secs_f64();
    let native_status = solved.status();
    let status = status(native_status)?;
    let solution = if solved.primal_solution_status() == HighsSolutionStatus::Feasible {
        let values = solved.get_solution();
        let dual_feasible = solved
            .int_info_value(c"dual_solution_status")
            .map_err(|s| Error::Internal(format!("HiGHS dual status: {s:?}")))?
            == HighsSolutionStatus::Feasible as i64;
        Some(Solution {
            objective: solved.objective_value(),
            variable_values: values.columns().to_vec(),
            constraint_activities: if request.diagnostics {
                values.rows().to_vec()
            } else {
                Vec::new()
            },
            dual: (request.diagnostics && dual_feasible).then(|| DualSolution {
                constraint_duals: values.dual_rows().to_vec(),
                reduced_costs: values.dual_columns().to_vec(),
            }),
        })
    } else {
        None
    };
    check_budget(deadline, cancel)?;
    tracing::info!(request_id = %request.request_id, ?status, validation_seconds, build_seconds, solve_seconds, "optimization completed");
    Ok(SolveResult {
        request_id: request.request_id,
        status: status as i32,
        solution,
        timings: Some(Timings {
            validation_seconds,
            build_seconds,
            solve_seconds,
        }),
        solver_status: format!("{native_status:?}"),
        execution: Some(crate::pb::ExecutionSettings {
            cpu_budget: config.cpu_budget as u32,
            solver_threads: config.solver_threads as u32,
            max_concurrent_solves: config.max_concurrent_solves as u32,
        }),
    })
}

fn range(lower: Option<f64>, upper: Option<f64>) -> (Bound<f64>, Bound<f64>) {
    (
        lower.map_or(Bound::Unbounded, Bound::Included),
        upper.map_or(Bound::Unbounded, Bound::Included),
    )
}

pub(crate) fn check_budget(deadline: Option<Instant>, cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if deadline.is_some_and(|d| Instant::now() >= d) {
        return Err(Error::Deadline);
    }
    Ok(())
}

fn status(status: HighsModelStatus) -> Result<SolveStatus> {
    use HighsModelStatus as H;
    Ok(match status {
        H::Optimal => SolveStatus::Optimal,
        H::Infeasible => SolveStatus::Infeasible,
        H::Unbounded => SolveStatus::Unbounded,
        H::UnboundedOrInfeasible => SolveStatus::InfeasibleOrUnbounded,
        H::ReachedTimeLimit => SolveStatus::TimeLimit,
        H::ReachedIterationLimit => SolveStatus::IterationLimit,
        H::ReachedInterrupt => SolveStatus::Interrupted,
        H::ObjectiveBound
        | H::ObjectiveTarget
        | H::ReachedSolutionLimit
        | H::ReachedMemoryLimit => SolveStatus::OtherLimit,
        H::LoadError | H::ModelError | H::PresolveError | H::SolveError | H::PostsolveError => {
            return Err(Error::Internal(format!("HiGHS status: {status:?}")));
        }
        _ => SolveStatus::Unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn active_simplex_and_ipm_solves_observe_cancellation_and_deadlines() {
        for algorithm in ["simplex", "ipm"] {
            for expire in [false, true] {
                let request =
                    crate::samples::generate(crate::samples::Family::Production, 100, 42).unwrap();
                let (control, entered, release) = TestControl::new(&request.request_id, algorithm);
                let cancel = CancellationToken::new();
                let token = cancel.clone();
                let deadline = expire.then(|| Instant::now() + Duration::from_secs(1));
                let hook = control.clone();
                let worker = tokio::task::spawn_blocking(move || {
                    solve_inner(request, &Config::default(), deadline, &token, Some(hook))
                });
                tokio::time::timeout(Duration::from_secs(5), entered)
                    .await
                    .expect("native callback must start")
                    .unwrap();
                if let Some(deadline) = deadline {
                    crate::grpc::wait_deadline(Some(deadline)).await;
                } else {
                    cancel.cancel();
                }
                release.send(()).unwrap();
                let result = tokio::time::timeout(Duration::from_secs(5), worker)
                    .await
                    .expect("native interruption must return")
                    .unwrap();
                if expire {
                    assert!(matches!(result, Err(Error::Deadline)), "{result:?}");
                } else {
                    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
                }
                assert_eq!(
                    *control.status.lock().unwrap(),
                    Some(HighsModelStatus::ReachedInterrupt),
                    "{algorithm} must stop inside HiGHS"
                );
            }
        }
    }

    #[test]
    fn status_mapping_preserves_limits_and_failures() {
        for (input, expected) in [
            (HighsModelStatus::ReachedTimeLimit, SolveStatus::TimeLimit),
            (
                HighsModelStatus::ReachedIterationLimit,
                SolveStatus::IterationLimit,
            ),
            (
                HighsModelStatus::UnboundedOrInfeasible,
                SolveStatus::InfeasibleOrUnbounded,
            ),
            (HighsModelStatus::ReachedInterrupt, SolveStatus::Interrupted),
            (
                HighsModelStatus::ReachedMemoryLimit,
                SolveStatus::OtherLimit,
            ),
        ] {
            assert_eq!(status(input).unwrap(), expected);
        }
        assert!(status(HighsModelStatus::SolveError).is_err());
    }
}
