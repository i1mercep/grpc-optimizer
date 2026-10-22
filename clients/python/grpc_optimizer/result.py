"""Python-facing solve results and validation before updating a Pyomo model."""

import math
from dataclasses import dataclass, field
from enum import IntEnum

import grpc

from ._generated.optimization.v1 import optimizer_pb2 as pb
from .adapter import PreparedModel


class SolveStatus(IntEnum):
    OPTIMAL = pb.SOLVE_STATUS_OPTIMAL
    INFEASIBLE = pb.SOLVE_STATUS_INFEASIBLE
    UNBOUNDED = pb.SOLVE_STATUS_UNBOUNDED
    INFEASIBLE_OR_UNBOUNDED = pb.SOLVE_STATUS_INFEASIBLE_OR_UNBOUNDED
    TIME_LIMIT = pb.SOLVE_STATUS_TIME_LIMIT
    ITERATION_LIMIT = pb.SOLVE_STATUS_ITERATION_LIMIT
    INTERRUPTED = pb.SOLVE_STATUS_INTERRUPTED
    OTHER_LIMIT = pb.SOLVE_STATUS_OTHER_LIMIT
    UNKNOWN = pb.SOLVE_STATUS_UNKNOWN


class ProtocolError(RuntimeError):
    """The server returned a response that cannot safely be applied to the model."""


_STATUS_CODES = {code.value[0]: code for code in grpc.StatusCode}


class JobError(RuntimeError):
    """The server rejected one GrpcOptimizer solve; the optimizer stays open."""

    def __init__(self, code: int, details: str) -> None:
        self.code = _STATUS_CODES.get(code, grpc.StatusCode.UNKNOWN)
        self.details = details
        super().__init__(f"{self.code.name}: {details}")


@dataclass(frozen=True)
class SolveResult:
    request_id: str
    status: SolveStatus
    objective: float | None
    has_solution: bool
    solution_loaded: bool
    solver_status: str
    raw: pb.SolveResult = field(repr=False, compare=False)

    @property
    def is_optimal(self) -> bool:
        return self.status == SolveStatus.OPTIMAL


def finish_solve(prepared: PreparedModel, response: pb.SolveResult, *, load_solution: bool) -> SolveResult:
    """Validate a response and restore the captured objective, including its offset.

    Check the full solution before optionally updating Pyomo variables; malformed
    responses raise ProtocolError without changing their existing values.
    """
    if response.request_id != prepared.request.request_id:
        raise ProtocolError("response request_id does not match the submitted model")
    try:
        status = SolveStatus(response.status)
    except ValueError as error:
        raise ProtocolError(f"unsupported solve status: {response.status}") from error
    has_solution = response.HasField("solution")
    if status == SolveStatus.OPTIMAL and not has_solution:
        raise ProtocolError("optimal response is missing its solution")
    objective = None
    if has_solution:
        values = tuple(response.solution.variable_values)
        if len(values) != len(prepared.variables):
            raise ProtocolError("solution variable count does not match the submitted model")
        if not all(math.isfinite(value) for value in values) or not math.isfinite(response.solution.objective):
            raise ProtocolError("solution contains nonfinite values")
        # Evaluate the captured objective even when loading is disabled. This
        # includes constants and fixed variables omitted from the wire format.
        objective = math.fsum(
            [prepared.objective_constant]
            + [var.objective * value for var, value in zip(prepared.request.model.variables, values, strict=True)]
        )
        if not math.isfinite(objective):
            raise ProtocolError("solution objective is nonfinite")
        # Validate the complete vector first, so malformed responses cannot
        # partially overwrite an existing model solution.
        if load_solution:
            for var, value in zip(prepared.variables, values, strict=True):
                var.set_value(value, skip_validation=True)
    return SolveResult(
        request_id=response.request_id,
        status=status,
        objective=objective,
        has_solution=has_solution,
        solution_loaded=has_solution and load_solution,
        solver_status=response.solver_status,
        raw=response,
    )


def finish_outcome(prepared: PreparedModel, outcome: pb.JobOutcome, *, load_solution: bool) -> SolveResult:
    """Raise JobError for a rejected job; otherwise finish it like finish_solve."""
    if outcome.request_id != prepared.request.request_id:
        raise ProtocolError("outcome request_id does not match the submitted job")
    if outcome.HasField("error"):
        raise JobError(outcome.error.code, outcome.error.message)
    if not outcome.HasField("result"):
        raise ProtocolError("outcome has neither a result nor an error")
    return finish_solve(prepared, outcome.result, load_solution=load_solution)
