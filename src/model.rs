//! Structural and resource validation before a wire model reaches HiGHS.

use std::collections::HashSet;

use crate::{
    config::Config,
    pb::{LinearProgram, ObjectiveSense, SolveInput, SolveRequest},
};

/// Request and execution failures; mathematical solve outcomes are returned in `SolveResult`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Exhausted(String),
    #[error("deadline exceeded")]
    Deadline,
    #[error("request cancelled")]
    Cancelled,
    #[error("server is shutting down")]
    Unavailable,
    #[error("{0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Checks sparse row indices, finite coefficients, bounds, and configured model limits.
///
/// Omitted bounds are unbounded. Duplicate indices within a row are rejected rather
/// than combined. Returns [`Error::Invalid`] for malformed input or [`Error::Exhausted`]
/// for resource limits; feasibility and boundedness are left to the solver.
pub fn validate(request: &SolveRequest, config: &Config) -> Result<()> {
    let invalid = |message: &str| Error::Invalid(message.to_owned());
    if request.request_id.is_empty() || request.request_id.len() > 128 {
        return Err(invalid("request_id must contain 1..128 UTF-8 bytes"));
    }
    if request
        .time_limit_seconds
        .is_some_and(|v| !v.is_finite() || v <= 0.0)
    {
        return Err(invalid("time limit must be finite and positive"));
    }
    let model = request
        .model
        .as_ref()
        .ok_or_else(|| invalid("model is required"))?;
    validate_model(model, config)
}

/// Applies the model checks of [`validate`] without request-level fields.
pub fn validate_model(model: &LinearProgram, config: &Config) -> Result<()> {
    let invalid = |message: &str| Error::Invalid(message.to_owned());
    if !matches!(
        ObjectiveSense::try_from(model.sense),
        Ok(ObjectiveSense::Minimize | ObjectiveSense::Maximize)
    ) {
        return Err(invalid("objective sense must be MINIMIZE or MAXIMIZE"));
    }
    if model.variables.is_empty() {
        return Err(invalid("at least one variable is required"));
    }
    if model.variables.len() > config.max_variables
        || model.constraints.len() > config.max_constraints
    {
        return Err(Error::Exhausted(
            "model dimensions exceed configured limits".into(),
        ));
    }
    for var in &model.variables {
        if !var.objective.is_finite() {
            return Err(invalid("objective coefficients must be finite"));
        }
        bounds(var.lower, var.upper)?;
    }
    let mut nonzeros = 0usize;
    let mut seen = HashSet::new();
    for row in &model.constraints {
        bounds(row.lower, row.upper)?;
        if row.indices.len() != row.coefficients.len() {
            return Err(invalid(
                "constraint indices and coefficients must have equal lengths",
            ));
        }
        nonzeros = nonzeros
            .checked_add(row.indices.len())
            .ok_or_else(|| Error::Exhausted("nonzero count overflow".into()))?;
        if nonzeros > config.max_nonzeros {
            return Err(Error::Exhausted(
                "nonzero count exceeds configured limit".into(),
            ));
        }
        seen.clear();
        for (&index, &coefficient) in row.indices.iter().zip(&row.coefficients) {
            if index as usize >= model.variables.len() {
                return Err(invalid("constraint variable index is out of range"));
            }
            if !coefficient.is_finite() {
                return Err(invalid("constraint coefficients must be finite"));
            }
            if !seen.insert(index) {
                return Err(invalid("duplicate variable index within constraint"));
            }
        }
    }
    Ok(())
}

/// Builds a job from a session model and sparse input overrides.
///
/// Checks override indices only; call [`validate`] on the result for values.
pub fn apply(base: &LinearProgram, input: SolveInput) -> Result<SolveRequest> {
    if input.objective_indices.len() != input.objective_values.len() {
        return Err(Error::Invalid(
            "objective indices and values must have equal lengths".into(),
        ));
    }
    let mut model = base.clone();
    let mut seen = HashSet::new();
    for (&index, &value) in input.objective_indices.iter().zip(&input.objective_values) {
        override_target(&mut model.variables, index, &mut seen, "objective")?.objective = value;
    }
    seen.clear();
    for bound in &input.variable_bounds {
        let var = override_target(
            &mut model.variables,
            bound.index,
            &mut seen,
            "variable bound",
        )?;
        (var.lower, var.upper) = (bound.lower, bound.upper);
    }
    seen.clear();
    for bound in &input.constraint_bounds {
        let row = override_target(
            &mut model.constraints,
            bound.index,
            &mut seen,
            "constraint bound",
        )?;
        (row.lower, row.upper) = (bound.lower, bound.upper);
    }
    Ok(SolveRequest {
        request_id: input.request_id,
        model: Some(model),
        diagnostics: input.diagnostics,
        time_limit_seconds: input.time_limit_seconds,
    })
}

/// Returns the item at `index`, rejecting out-of-range and repeated indices.
fn override_target<'a, T>(
    items: &'a mut [T],
    index: u32,
    seen: &mut HashSet<u32>,
    kind: &str,
) -> Result<&'a mut T> {
    if !seen.insert(index) {
        return Err(Error::Invalid(format!("duplicate {kind} index")));
    }
    items
        .get_mut(index as usize)
        .ok_or_else(|| Error::Invalid(format!("{kind} index is out of range")))
}

/// Validates inclusive finite bounds, using `None` for either unbounded side.
fn bounds(lower: Option<f64>, upper: Option<f64>) -> Result<()> {
    if lower.is_some_and(|x| !x.is_finite()) || upper.is_some_and(|x| !x.is_finite()) {
        return Err(Error::Invalid(
            "explicit bounds must be finite; omit unbounded sides".into(),
        ));
    }
    if matches!((lower, upper), (Some(l), Some(u)) if l > u) {
        return Err(Error::Invalid("lower bound exceeds upper bound".into()));
    }
    Ok(())
}
