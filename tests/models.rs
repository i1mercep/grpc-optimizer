use std::time::Instant;

use grpc_optimizer::{
    config::Config,
    engine::Engine,
    model::{self, Error},
    pb::{BoundOverride, ObjectiveSense, Problem, SolveInput, SolveRequest, SolveStatus},
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

/// Objective gradient `c + Qx` for terms `q * x_i * x_j`.
fn gradient(model: &Problem, x: &[f64]) -> Vec<f64> {
    let mut gradient: Vec<f64> = model.variables.iter().map(|v| v.objective).collect();
    let pairs = std::iter::zip(&model.quadratic_rows, &model.quadratic_columns);
    for ((&i, &j), &q) in pairs.zip(&model.quadratic_coefficients) {
        gradient[i as usize] += q * x[j as usize];
        gradient[j as usize] += q * x[i as usize];
    }
    gradient
}

fn check_solution(request: &SolveRequest, result: &grpc_optimizer::pb::SolveResult) {
    let Some(solution) = &result.solution else {
        return;
    };
    let model = request.model.as_ref().unwrap();
    let x = &solution.variable_values;
    assert_eq!(x.len(), model.variables.len());
    let mut objective = 0.0;
    for (variable, &value) in model.variables.iter().zip(x) {
        assert!(value.is_finite());
        assert!(value >= variable.lower.unwrap_or(f64::NEG_INFINITY) - 1e-6);
        assert!(value <= variable.upper.unwrap_or(f64::INFINITY) + 1e-6);
        objective += value * variable.objective;
    }
    let pairs = std::iter::zip(&model.quadratic_rows, &model.quadratic_columns);
    for ((&i, &j), &q) in pairs.zip(&model.quadratic_coefficients) {
        objective += q * x[i as usize] * x[j as usize];
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
        for (j, gradient) in gradient(model, x).into_iter().enumerate() {
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
            assert_close(gradient - aty, dual.reduced_costs[j]);
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
fn legacy_linear_program_and_problem_names_are_compatible() {
    let legacy: grpc_optimizer::pb::LinearProgram = samples::fixture("production")
        .unwrap()
        .request
        .model
        .unwrap();
    let problem: Problem = legacy;
    assert_eq!(problem.variables.len(), 2);
    assert!(!problem.is_quadratic());
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
fn quadratic_validation_rejects_malformed_terms_and_negative_curvature() {
    let config = Config::default();
    let base = samples::fixture("quadratic").unwrap().request;
    model::validate(&base, &config).unwrap();
    type Mutation = Box<dyn Fn(&mut Problem)>;
    let mutations: Vec<Mutation> = vec![
        Box::new(|m| m.quadratic_rows.truncate(1)),
        Box::new(|m| m.quadratic_columns[0] = 2),
        Box::new(|m| m.quadratic_coefficients[1] = f64::NAN),
        Box::new(|m| {
            m.quadratic_rows.push(1);
            m.quadratic_columns.push(0);
            m.quadratic_coefficients.push(1.0);
        }),
        Box::new(|m| m.quadratic_coefficients[0] = -1.0),
        Box::new(|m| m.sense = ObjectiveSense::Maximize as i32),
    ];
    for mutate in mutations {
        let mut request = base.clone();
        mutate(request.model.as_mut().unwrap());
        assert!(matches!(
            model::validate(&request, &config),
            Err(Error::Invalid(_))
        ));
    }
    let config = Config {
        max_nonzeros: 4,
        ..config
    };
    assert!(matches!(
        model::validate(&base, &config),
        Err(Error::Exhausted(_))
    ));
}

#[test]
fn bounded_indefinite_quadratic_objectives_are_invalid() {
    for sense in [ObjectiveSense::Minimize, ObjectiveSense::Maximize] {
        for cross in [-3.0, 3.0] {
            let sign = if sense == ObjectiveSense::Minimize {
                1.0
            } else {
                -1.0
            };
            let mut request = samples::fixture("quadratic").unwrap().request;
            let model = request.model.as_mut().unwrap();
            model.sense = sense as i32;
            model.constraints.clear();
            model.quadratic_coefficients = vec![sign, cross, sign];
            for variable in &mut model.variables {
                variable.objective = 0.0;
                variable.lower = Some(-1.0);
                variable.upper = Some(1.0);
            }
            let result =
                solver::solve(request, &Config::default(), None, &CancellationToken::new());
            assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
        }
    }
}

#[test]
fn coupled_quadratic_curvature_is_checked_beyond_variable_pairs() {
    for (cross, valid) in [(-2.0, false), (2.0, true)] {
        let mut request = samples::fixture("quadratic").unwrap().request;
        let model = request.model.as_mut().unwrap();
        model.variables.push(Default::default());
        model.constraints.clear();
        model.quadratic_rows = vec![0, 0, 0, 1, 1, 2];
        model.quadratic_columns = vec![0, 1, 2, 1, 2, 2];
        model.quadratic_coefficients = vec![1.0, 2.0, 2.0, 1.0, cross, 1.0];
        for variable in &mut model.variables {
            variable.objective = 0.0;
            variable.lower = Some(-1.0);
            variable.upper = Some(1.0);
        }
        let result = solver::solve(request, &Config::default(), None, &CancellationToken::new());
        if valid {
            assert_eq!(result.unwrap().status, SolveStatus::Optimal as i32);
        } else {
            assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
        }
    }
}

#[test]
fn quadratic_curvature_check_caps_factor_fill_in() {
    // A 3D grid coupling fills its Cholesky factor far beyond its own nonzeros.
    const SIDE: u32 = 16;
    let index = |x: u32, y: u32, z: u32| (x * SIDE + y) * SIDE + z;
    let mut request = samples::fixture("quadratic").unwrap().request;
    let model = request.model.as_mut().unwrap();
    model.constraints.clear();
    model.variables = (0..SIDE.pow(3))
        .map(|_| grpc_optimizer::pb::Variable {
            lower: Some(-1.0),
            upper: Some(1.0),
            ..Default::default()
        })
        .collect();
    let (mut rows, mut columns, mut coefficients) = (vec![], vec![], vec![]);
    for x in 0..SIDE {
        for y in 0..SIDE {
            for z in 0..SIDE {
                let i = index(x, y, z);
                let neighbors = [(x + 1, y, z), (x, y + 1, z), (x, y, z + 1)];
                rows.push(i);
                columns.push(i);
                coefficients.push(6.0);
                for (nx, ny, nz) in neighbors {
                    if nx < SIDE && ny < SIDE && nz < SIDE {
                        rows.push(i);
                        columns.push(index(nx, ny, nz));
                        coefficients.push(-1.0);
                    }
                }
            }
        }
    }
    let terms = coefficients.len();
    (
        model.quadratic_rows,
        model.quadratic_columns,
        model.quadratic_coefficients,
    ) = (rows, columns, coefficients);
    let tight = Config {
        max_nonzeros: terms,
        ..Config::default()
    };
    let result = solver::solve(request.clone(), &tight, None, &CancellationToken::new());
    assert!(
        matches!(&result, Err(Error::Exhausted(message)) if message.contains("factorization")),
        "{result:?}"
    );
    let result = solver::solve(request, &Config::default(), None, &CancellationToken::new());
    assert_eq!(result.unwrap().status, SolveStatus::Optimal as i32);
}

#[test]
fn mixed_scale_indefinite_quadratic_objective_is_invalid() {
    let mut request = samples::fixture("quadratic").unwrap().request;
    let model = request.model.as_mut().unwrap();
    model.variables.push(Default::default());
    model.constraints.clear();
    model.quadratic_rows = vec![0, 1, 1, 2];
    model.quadratic_columns = vec![0, 1, 2, 2];
    model.quadratic_coefficients = vec![1e14, 1.0, 3.0, 1.0];
    let result = solver::solve(request, &Config::default(), None, &CancellationToken::new());
    assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
}

#[test]
fn semidefinite_quadratic_objectives_are_supported() {
    for sense in [ObjectiveSense::Minimize, ObjectiveSense::Maximize] {
        let sign = if sense == ObjectiveSense::Minimize {
            1.0
        } else {
            -1.0
        };
        let mut request = samples::fixture("quadratic").unwrap().request;
        let model = request.model.as_mut().unwrap();
        model.sense = sense as i32;
        model.quadratic_coefficients = vec![sign, 2.0 * sign, sign];
        for variable in &mut model.variables {
            variable.objective = -sign;
            variable.lower = Some(-1.0);
            variable.upper = Some(1.0);
        }
        let result = solver::solve(
            request.clone(),
            &Config::default(),
            None,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(result.status, SolveStatus::Optimal as i32);
        check_solution(&request, &result);
    }
}

#[test]
fn indefinite_quadratic_objective_is_invalid() {
    let mut request = samples::fixture("quadratic").unwrap().request;
    let model = request.model.as_mut().unwrap();
    // x^2 + y^2 - 3xy has negative curvature along the feasible line x = y.
    model.quadratic_coefficients = vec![1.0, -3.0, 1.0];
    model.constraints[0].coefficients = vec![1.0, -1.0];
    model.constraints[0].lower = Some(0.0);
    model.constraints[0].upper = Some(0.0);
    let result = solver::solve(request, &Config::default(), None, &CancellationToken::new());
    assert!(matches!(result, Err(Error::Invalid(_))), "{result:?}");
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
    let mut request = samples::fixture("quadratic").unwrap().request;
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
