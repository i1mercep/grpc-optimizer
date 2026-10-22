use std::time::Instant;

use grpc_optimizer::{
    config::Config,
    engine::Engine,
    model::{self, Error},
    pb::{BoundOverride, SolveInput, SolveRequest, SolveStatus},
    samples::{self, Family},
    solver,
};
use tokio_util::sync::CancellationToken;

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-6 * (1.0 + expected.abs()),
        "{actual} != {expected}"
    );
}

fn check_solution(request: &SolveRequest, result: &grpc_optimizer::pb::SolveResult) {
    let Some(solution) = &result.solution else {
        return;
    };
    let model = request.model.as_ref().unwrap();
    assert_eq!(solution.variable_values.len(), model.variables.len());
    let mut objective = 0.0;
    for (variable, &value) in model.variables.iter().zip(&solution.variable_values) {
        assert!(value.is_finite());
        assert!(value >= variable.lower.unwrap_or(f64::NEG_INFINITY) - 1e-6);
        assert!(value <= variable.upper.unwrap_or(f64::INFINITY) + 1e-6);
        objective += value * variable.objective;
    }
    assert_close(solution.objective, objective);
    for (i, row) in model.constraints.iter().enumerate() {
        let activity: f64 = row
            .indices
            .iter()
            .zip(&row.coefficients)
            .map(|(&j, &a)| a * solution.variable_values[j as usize])
            .sum();
        assert!(activity >= row.lower.unwrap_or(f64::NEG_INFINITY) - 1e-6);
        assert!(activity <= row.upper.unwrap_or(f64::INFINITY) + 1e-6);
        if request.diagnostics {
            assert_close(solution.constraint_activities[i], activity);
        }
    }
    if let Some(dual) = &solution.dual {
        assert_eq!(dual.reduced_costs.len(), model.variables.len());
        assert_eq!(dual.constraint_duals.len(), model.constraints.len());
        for (j, var) in model.variables.iter().enumerate() {
            let aty: f64 = model
                .constraints
                .iter()
                .zip(&dual.constraint_duals)
                .map(|(row, y)| {
                    row.indices
                        .iter()
                        .zip(&row.coefficients)
                        .filter(|(index, _)| **index as usize == j)
                        .map(|(_, a)| a * y)
                        .sum::<f64>()
                })
                .sum();
            assert_close(var.objective - aty, dual.reduced_costs[j]);
        }
    }
}

#[tokio::test]
async fn fixtures_have_known_optima_feasibility_and_dual_stationarity() {
    let engine = Engine::new(Config::default()).unwrap();
    for sample in samples::fixtures() {
        let request = sample.request;
        let result = engine
            .solve(request.clone(), None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(result.status, sample.expected_status, "{}", sample.name);
        if let Some(expected) = sample.expected_objective {
            let solution = result.solution.as_ref().unwrap();
            assert_close(solution.objective, expected);
            for (&actual, &expected) in solution
                .variable_values
                .iter()
                .zip(sample.expected_values.as_ref().unwrap())
            {
                assert_close(actual, expected);
            }
            assert!(solution.dual.is_some());
        }
        if result.status == SolveStatus::Infeasible as i32 {
            assert!(result.solution.is_none());
        }
        check_solution(&request, &result);
        let timings = result.timings.unwrap();
        assert!(
            timings.validation_seconds >= 0.0
                && timings.build_seconds >= 0.0
                && timings.solve_seconds >= 0.0
        );
    }
    engine.shutdown();
    engine.drain().await;
}

#[test]
fn validation_rejects_malformed_inputs_and_model_limits() {
    let config = Config::default();
    let base = samples::fixture("production").unwrap().request;
    type Mutation = Box<dyn Fn(&mut SolveRequest)>;
    let mutations: Vec<Mutation> = vec![
        Box::new(|r| r.request_id.clear()),
        Box::new(|r| r.request_id = "a".repeat(129)),
        Box::new(|r| r.model = None),
        Box::new(|r| r.time_limit_seconds = Some(f64::NAN)),
        Box::new(|r| r.time_limit_seconds = Some(0.0)),
        Box::new(|r| r.model.as_mut().unwrap().sense = 0),
        Box::new(|r| r.model.as_mut().unwrap().sense = 100),
        Box::new(|r| r.model.as_mut().unwrap().variables.clear()),
        Box::new(|r| r.model.as_mut().unwrap().variables[0].objective = f64::INFINITY),
        Box::new(|r| r.model.as_mut().unwrap().variables[0].upper = Some(-1.0)),
        Box::new(|r| r.model.as_mut().unwrap().variables[0].lower = Some(f64::NEG_INFINITY)),
        Box::new(|r| r.model.as_mut().unwrap().constraints[0].indices[0] = 2),
        Box::new(|r| r.model.as_mut().unwrap().constraints[0].indices[0] = 1),
        Box::new(|r| {
            r.model.as_mut().unwrap().constraints[0]
                .coefficients
                .pop()
                .map(|_| ())
                .unwrap()
        }),
        Box::new(|r| r.model.as_mut().unwrap().constraints[0].coefficients[0] = f64::NAN),
        Box::new(|r| r.model.as_mut().unwrap().constraints[0].lower = Some(101.0)),
    ];
    for mutate in mutations {
        let mut request = base.clone();
        mutate(&mut request);
        assert!(matches!(
            model::validate(&request, &config),
            Err(Error::Invalid(_))
        ));
    }
    for config in [
        Config {
            max_variables: 1,
            ..config.clone()
        },
        Config {
            max_constraints: 1,
            ..config.clone()
        },
        Config {
            max_nonzeros: 1,
            ..config.clone()
        },
    ] {
        assert!(matches!(
            model::validate(&base, &config),
            Err(Error::Exhausted(_))
        ));
    }
}

#[test]
fn session_overrides_replace_values_and_reject_bad_indices() {
    let base = samples::fixture("production")
        .unwrap()
        .request
        .model
        .unwrap();
    let input = SolveInput {
        request_id: "a".into(),
        diagnostics: true,
        time_limit_seconds: Some(1.0),
        objective_indices: vec![1],
        objective_values: vec![50.0],
        variable_bounds: vec![BoundOverride {
            index: 0,
            lower: None,
            upper: Some(10.0),
        }],
        constraint_bounds: vec![BoundOverride {
            index: 1,
            lower: Some(1.0),
            upper: None,
        }],
    };
    let request = model::apply(&base, input.clone()).unwrap();
    assert_eq!(request.request_id, "a");
    assert!(request.diagnostics);
    assert_eq!(request.time_limit_seconds, Some(1.0));
    let applied = request.model.unwrap();
    assert_eq!(applied.variables[0].objective, 40.0);
    assert_eq!(applied.variables[1].objective, 50.0);
    assert_eq!(
        (applied.variables[0].lower, applied.variables[0].upper),
        (None, Some(10.0))
    );
    assert_eq!(applied.variables[1].lower, base.variables[1].lower);
    assert_eq!(
        (applied.constraints[1].lower, applied.constraints[1].upper),
        (Some(1.0), None)
    );
    assert_eq!(applied.constraints[0], base.constraints[0]);

    let duplicate = || BoundOverride {
        index: 0,
        lower: None,
        upper: None,
    };
    type Mutation = Box<dyn Fn(&mut SolveInput)>;
    let mutations: Vec<Mutation> = vec![
        Box::new(|i| i.objective_values.clear()),
        Box::new(|i| i.objective_indices[0] = 2),
        Box::new(|i| {
            i.objective_indices.push(1);
            i.objective_values.push(1.0);
        }),
        Box::new(|i| i.variable_bounds[0].index = 2),
        Box::new(move |i| i.variable_bounds.push(duplicate())),
        Box::new(|i| i.constraint_bounds[0].index = 2),
        Box::new(move |i| {
            i.constraint_bounds[0].index = 0;
            i.constraint_bounds.push(duplicate());
        }),
    ];
    for mutate in mutations {
        let mut bad = input.clone();
        mutate(&mut bad);
        assert!(matches!(model::apply(&base, bad), Err(Error::Invalid(_))));
    }
}

#[tokio::test]
async fn diagnostics_are_opt_in_and_unbounded_lower_is_preserved() {
    let engine = Engine::new(Config::default()).unwrap();
    let mut request = samples::fixture("production").unwrap().request;
    request.diagnostics = false;
    let result = engine
        .solve(request.clone(), None, CancellationToken::new())
        .await
        .unwrap();
    let solution = result.solution.unwrap();
    assert!(solution.dual.is_none() && solution.constraint_activities.is_empty());
    request.model.as_mut().unwrap().variables[0].lower = None;
    request.model.as_mut().unwrap().variables[0].upper = Some(-2.0);
    let result = engine
        .solve(request, None, CancellationToken::new())
        .await
        .unwrap();
    assert!(result.solution.unwrap().variable_values[0] <= -2.0);
    assert!(matches!(
        engine
            .solve(
                samples::fixture("production").unwrap().request,
                Some(Instant::now()),
                CancellationToken::new()
            )
            .await,
        Err(Error::Deadline)
    ));
}

#[test]
fn generators_are_reproducible_and_solver_limit_is_observable() {
    let config = Config::default();
    for family in [Family::Production, Family::Transportation] {
        let request = samples::generate(family, 20, 42).unwrap();
        assert_eq!(request, samples::generate(family, 20, 42).unwrap());
        let result =
            solver::solve(request.clone(), &config, None, &CancellationToken::new()).unwrap();
        assert_eq!(result.status, SolveStatus::Optimal as i32);
        check_solution(&request, &result);
    }
    let mut request = samples::generate(Family::Transportation, 100, 42).unwrap();
    request.time_limit_seconds = Some(1e-12);
    let result = solver::solve(request.clone(), &config, None, &CancellationToken::new()).unwrap();
    assert_eq!(result.status, SolveStatus::TimeLimit as i32);
    check_solution(&request, &result);
}

#[test]
fn invalid_cpu_and_time_configuration_is_rejected() {
    assert!(
        Config {
            max_concurrent_solves: 0,
            ..Config::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        Config {
            solver_threads: usize::MAX,
            ..Config::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        Config {
            max_time_limit_seconds: 301.0,
            ..Config::default()
        }
        .validate()
        .is_err()
    );
}
