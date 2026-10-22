"""Convert a continuous linear Pyomo model to the optimizer's protobuf format."""

import math
from dataclasses import dataclass
from typing import Any

import pyomo.environ as pyo
from pyomo.common.collections import ComponentMap
from pyomo.repn.standard_repn import generate_standard_repn

from ._generated.optimization.v1 import optimizer_pb2 as pb


@dataclass(frozen=True)
class PreparedModel:
    request: pb.SolveRequest
    variables: tuple[Any, ...]
    objective_constant: float


def _number(value: Any, name: str) -> float:
    """Evaluate a Pyomo numeric value, rejecting nonfinite results by name."""
    result = float(pyo.value(value))
    if not math.isfinite(result):
        raise ValueError(f"{name} must be finite")
    return result


def _linear(expression: Any, name: str) -> Any:
    """Expand an expression with fixed values substituted; reject nonlinear terms."""
    repn = generate_standard_repn(expression)
    if not repn.is_linear():
        raise ValueError(f"{name} must be linear")
    return repn


def prepare_model(model: Any, request_id: str) -> PreparedModel:
    """Capture the wire model and the information needed to restore a solution.

    Fixed variables are substituted into expressions. Constraint constants are
    moved to the bounds. Objective constants are captured locally because the
    wire format omits offsets; the result layer restores them for callers.
    """
    if not isinstance(model, pyo.Block) or not model.is_constructed():
        raise ValueError("a constructed Pyomo model is required")
    if not request_id or len(request_id.encode("utf-8")) > 128:
        raise ValueError("request_id must contain 1..128 UTF-8 bytes")

    supported = {
        pyo.Block,
        pyo.Var,
        pyo.Constraint,
        pyo.Objective,
        pyo.Param,
        pyo.Set,
        pyo.RangeSet,
        pyo.Expression,
        pyo.Suffix,
    }
    for component in model.component_objects(active=True, descend_into=True):
        if component.ctype not in supported:
            raise ValueError(f"unsupported component {component.name}: {component.ctype.__name__}")

    objectives = list(model.component_data_objects(pyo.Objective, active=True))
    if len(objectives) != 1:
        raise ValueError("exactly one active objective is required")
    objective = objectives[0]
    repn = _linear(objective.expr, objective.name)
    objective_constant = _number(repn.constant, f"{objective.name} constant")
    lp = pb.LinearProgram(
        sense=(pb.OBJECTIVE_SENSE_MINIMIZE if objective.sense == pyo.minimize else pb.OBJECTIVE_SENSE_MAXIMIZE)
    )

    variables = []
    indices = ComponentMap()
    for var in model.component_data_objects(pyo.Var, active=True):
        if not var.is_continuous():
            raise ValueError(f"{var.name} must be continuous; integer and binary variables are unsupported")
        lower = None if var.lb is None else _number(var.lb, f"{var.name} lower bound")
        upper = None if var.ub is None else _number(var.ub, f"{var.name} upper bound")
        if lower is not None and upper is not None and lower > upper:
            raise ValueError(f"{var.name} lower bound exceeds upper bound")
        if var.fixed:
            fixed = _number(var, f"{var.name} fixed value")
            if (lower is not None and fixed < lower) or (upper is not None and fixed > upper):
                raise ValueError(f"{var.name} fixed value is outside its bounds")
            continue
        indices[var] = len(variables)
        variables.append(var)
        lp.variables.add(lower=lower, upper=upper)
    if not variables:
        raise ValueError("at least one unfixed variable is required")

    def index(var: Any) -> int:
        if var not in indices:
            raise ValueError(f"{var.name} is outside the active model")
        return indices[var]

    for var, coefficient in zip(repn.linear_vars, repn.linear_coefs, strict=True):
        lp.variables[index(var)].objective = _number(coefficient, objective.name)

    for constraint in model.component_data_objects(pyo.Constraint, active=True):
        repn = _linear(constraint.body, constraint.name)
        constant = _number(repn.constant, f"{constraint.name} constant")
        row = lp.constraints.add()
        for var, coefficient in zip(repn.linear_vars, repn.linear_coefs, strict=True):
            row.indices.append(index(var))
            row.coefficients.append(_number(coefficient, constraint.name))
        if constraint.lower is not None:
            row.lower = _number(pyo.value(constraint.lower) - constant, f"{constraint.name} lower bound")
        if constraint.upper is not None:
            row.upper = _number(pyo.value(constraint.upper) - constant, f"{constraint.name} upper bound")

    return PreparedModel(pb.SolveRequest(request_id=request_id, model=lp), tuple(variables), objective_constant)


def _bounds(message: Any) -> tuple[float | None, float | None]:
    return (
        message.lower if message.HasField("lower") else None,
        message.upper if message.HasField("upper") else None,
    )


def session_input(base: pb.LinearProgram, request: pb.SolveRequest) -> pb.SolveInput:
    """Encode a request as overrides of the base model, rejecting structural changes.

    Only objective coefficients, variable bounds, and constraint bounds may differ.
    """
    model = request.model
    if model.sense != base.sense:
        raise ValueError("objective sense changed; create a new optimizer")
    if len(model.variables) != len(base.variables) or len(model.constraints) != len(base.constraints):
        raise ValueError("variable or constraint count changed; create a new optimizer")
    job = pb.SolveInput(request_id=request.request_id, diagnostics=request.diagnostics)
    if request.HasField("time_limit_seconds"):
        job.time_limit_seconds = request.time_limit_seconds
    for index, (var, old) in enumerate(zip(model.variables, base.variables, strict=True)):
        if var.objective != old.objective:
            job.objective_indices.append(index)
            job.objective_values.append(var.objective)
        if (bounds := _bounds(var)) != _bounds(old):
            job.variable_bounds.add(index=index, lower=bounds[0], upper=bounds[1])
    for index, (row, old) in enumerate(zip(model.constraints, base.constraints, strict=True)):
        if row.indices != old.indices or row.coefficients != old.coefficients:
            raise ValueError("constraint coefficients changed; create a new optimizer")
        if (bounds := _bounds(row)) != _bounds(old):
            job.constraint_bounds.add(index=index, lower=bounds[0], upper=bounds[1])
    return job


def to_solve_request(model: Any, request_id: str = "pyomo") -> tuple[pb.SolveRequest, list[Any]]:
    """Return a wire request and its variables in solution order, omitting objective offsets.

    Prefer grpc_optimizer.solve to restore offsets and load returned values.
    """
    prepared = prepare_model(model, request_id)
    return prepared.request, list(prepared.variables)
