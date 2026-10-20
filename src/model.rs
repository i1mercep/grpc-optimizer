use std::collections::HashSet;

use crate::{
    config::Config,
    pb::{ObjectiveSense, SolveRequest},
};

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
