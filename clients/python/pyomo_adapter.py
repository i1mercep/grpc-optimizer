"""Convert a continuous linear Pyomo model to the optimizer's protobuf format."""

import math
from pathlib import Path
import sys

import pyomo.environ as pyo
from pyomo.common.collections import ComponentMap
from pyomo.repn.standard_repn import generate_standard_repn

sys.path.insert(0, str(Path(__file__).parent / "generated"))
from optimization.v1 import optimizer_pb2 as pb


def _number(value, name):
    result = float(pyo.value(value))
    if not math.isfinite(result):
        raise ValueError(f"{name} must be finite")
    return result


def _linear(expression, name):
    repn = generate_standard_repn(expression)
    if not repn.is_linear():
        raise ValueError(f"{name} must be linear")
    return repn


def to_solve_request(model, request_id="pyomo"):
    """Return (SolveRequest, ordered unfixed variables) for a constructed model.

    Fixed variables are substituted into expressions. Constraint constants are
    moved to the bounds. Objective constants stay in Pyomo: evaluate the original
    objective after loading the returned variable values to include the offset.
    """
    if not isinstance(model, pyo.Block) or not model.is_constructed():
        raise ValueError("a constructed Pyomo model is required")
    if not request_id or len(request_id.encode("utf-8")) > 128:
        raise ValueError("request_id must contain 1..128 UTF-8 bytes")

    supported = {pyo.Block, pyo.Var, pyo.Constraint, pyo.Objective, pyo.Param,
                 pyo.Set, pyo.RangeSet, pyo.Expression, pyo.Suffix}
    for component in model.component_objects(active=True, descend_into=True):
        if component.ctype not in supported:
            raise ValueError(f"unsupported component {component.name}: "
                             f"{component.ctype.__name__}")

    objectives = list(model.component_data_objects(pyo.Objective, active=True))
    if len(objectives) != 1:
        raise ValueError("exactly one active objective is required")
    objective = objectives[0]
    repn = _linear(objective.expr, objective.name)
    _number(repn.constant, f"{objective.name} constant")
    lp = pb.LinearProgram(sense=(pb.OBJECTIVE_SENSE_MINIMIZE
                                if objective.sense == pyo.minimize
                                else pb.OBJECTIVE_SENSE_MAXIMIZE))

    variables = []
    indices = ComponentMap()
    for var in model.component_data_objects(pyo.Var, active=True):
        if not var.is_continuous():
            raise ValueError(f"{var.name} must be continuous; integer and binary "
                             "variables are unsupported")
        lower = None if var.lb is None else _number(var.lb, f"{var.name} lower bound")
        upper = None if var.ub is None else _number(var.ub, f"{var.name} upper bound")
        if lower is not None and upper is not None and lower > upper:
            raise ValueError(f"{var.name} lower bound exceeds upper bound")
        if var.fixed:
            fixed = _number(var, f"{var.name} fixed value")
            if ((lower is not None and fixed < lower)
                    or (upper is not None and fixed > upper)):
                raise ValueError(f"{var.name} fixed value is outside its bounds")
            continue
        indices[var] = len(variables)
        variables.append(var)
        lp.variables.add(lower=lower, upper=upper)
    if not variables:
        raise ValueError("at least one unfixed variable is required")

    def index(var):
        if var not in indices:
            raise ValueError(f"{var.name} is outside the active model")
        return indices[var]

    for var, coefficient in zip(repn.linear_vars, repn.linear_coefs):
        lp.variables[index(var)].objective = _number(coefficient, objective.name)

    for constraint in model.component_data_objects(pyo.Constraint, active=True):
        repn = _linear(constraint.body, constraint.name)
        constant = _number(repn.constant, f"{constraint.name} constant")
        row = lp.constraints.add()
        for var, coefficient in zip(repn.linear_vars, repn.linear_coefs):
            row.indices.append(index(var))
            row.coefficients.append(_number(coefficient, constraint.name))
        if constraint.lower is not None:
            row.lower = _number(pyo.value(constraint.lower) - constant,
                                f"{constraint.name} lower bound")
        if constraint.upper is not None:
            row.upper = _number(pyo.value(constraint.upper) - constant,
                                f"{constraint.name} upper bound")

    return pb.SolveRequest(request_id=request_id, model=lp), variables
