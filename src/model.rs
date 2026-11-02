//! Structural and resource validation before a wire model reaches HiGHS.

use std::collections::HashSet;

use faer::{
    Side,
    sparse::{SparseColMat, Triplet, linalg::LltError},
};

use crate::{
    config::Config,
    pb::{ObjectiveSense, Problem, SolveInput, SolveRequest},
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

impl Problem {
    pub fn is_quadratic(&self) -> bool {
        !self.quadratic_coefficients.is_empty()
    }
}

/// Checks sparse row and quadratic indices, finite coefficients, bounds, and configured
/// model limits.
///
/// Omitted bounds are unbounded. Duplicate indices within a row and duplicate quadratic
/// pairs are rejected rather than combined. Returns [`Error::Invalid`] for malformed input
/// or [`Error::Exhausted`] for resource limits. Full curvature is checked during native
/// model construction; feasibility and boundedness are left to the solver.
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
pub fn validate_model(model: &Problem, config: &Config) -> Result<()> {
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
    validate_quadratic(model, nonzeros, config)
}

/// Checks quadratic terms, which share the nonzero budget with constraint rows.
///
/// Native model construction separately checks curvature for coupled terms.
fn validate_quadratic(model: &Problem, nonzeros: usize, config: &Config) -> Result<()> {
    let invalid = |message: &str| Error::Invalid(message.to_owned());
    let terms = model.quadratic_coefficients.len();
    if model.quadratic_rows.len() != terms || model.quadratic_columns.len() != terms {
        return Err(invalid(
            "quadratic rows, columns, and coefficients must have equal lengths",
        ));
    }
    if nonzeros.saturating_add(terms) > config.max_nonzeros {
        return Err(Error::Exhausted(
            "nonzero count exceeds configured limit".into(),
        ));
    }
    let sign = if model.sense == ObjectiveSense::Maximize as i32 {
        -1.0
    } else {
        1.0
    };
    let mut seen = HashSet::with_capacity(terms);
    let pairs = std::iter::zip(&model.quadratic_rows, &model.quadratic_columns);
    for ((&row, &column), &coefficient) in pairs.zip(&model.quadratic_coefficients) {
        let pair = (row.min(column), row.max(column));
        if pair.1 as usize >= model.variables.len() {
            return Err(invalid("quadratic variable index is out of range"));
        }
        if !coefficient.is_finite() {
            return Err(invalid("quadratic coefficients must be finite"));
        }
        if !seen.insert(pair) {
            return Err(invalid("duplicate variable pair in quadratic objective"));
        }
        if row == column && sign * coefficient < 0.0 {
            return Err(invalid(
                "quadratic objective must be convex when minimizing and concave when maximizing",
            ));
        }
    }
    Ok(())
}

/// Equilibrates the objective matrix before testing it with a diagonal tolerance of 1e-12.
pub(crate) fn validate_curvature(model: &Problem) -> Result<()> {
    let pairs = std::iter::zip(&model.quadratic_rows, &model.quadratic_columns);
    if !pairs
        .zip(&model.quadratic_coefficients)
        .any(|((&row, &column), &value)| row != column && value != 0.0)
    {
        return Ok(());
    }
    let sign = if model.sense == ObjectiveSense::Maximize as i32 {
        -1.0
    } else {
        1.0
    };
    let dimension = model.variables.len();
    let mut diagonal = vec![0.0_f64; dimension];
    let pairs = std::iter::zip(&model.quadratic_rows, &model.quadratic_columns);
    for ((&row, &column), &coefficient) in pairs.zip(&model.quadratic_coefficients) {
        if row == column {
            diagonal[row as usize] = (sign * coefficient).sqrt();
        }
    }
    let mut entries = Vec::with_capacity(dimension + model.quadratic_coefficients.len());
    entries.extend((0..dimension).map(|index| Triplet::new(index, index, 1.0 + 1e-12)));
    let pairs = std::iter::zip(&model.quadratic_rows, &model.quadratic_columns);
    for ((&row, &column), &coefficient) in pairs.zip(&model.quadratic_coefficients) {
        if row == column || coefficient == 0.0 {
            continue;
        }
        let value = sign * (coefficient / diagonal[row as usize]) * 0.5 / diagonal[column as usize];
        if !value.is_finite() || value.abs() > 1.0 + 1e-12 {
            return Err(Error::Invalid(
                "quadratic objective must be convex when minimizing and concave when maximizing"
                    .into(),
            ));
        }
        entries.push(Triplet::new(
            row.max(column) as usize,
            row.min(column) as usize,
            value,
        ));
    }
    let matrix = SparseColMat::<usize, f64>::try_new_from_triplets(dimension, dimension, &entries)
        .map_err(|error| Error::Exhausted(format!("quadratic validation matrix: {error}")))?;
    match matrix.sp_cholesky(Side::Lower) {
        Ok(_) => Ok(()),
        Err(LltError::Numeric(_)) => Err(Error::Invalid(
            "quadratic objective must be convex when minimizing and concave when maximizing".into(),
        )),
        Err(LltError::Generic(error)) => {
            Err(Error::Exhausted(format!("quadratic validation: {error}")))
        }
    }
}

/// Builds a job from a session model and sparse input overrides.
///
/// Checks override indices only; call [`validate`] on the result for values.
pub fn apply(base: &Problem, input: SolveInput) -> Result<SolveRequest> {
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
