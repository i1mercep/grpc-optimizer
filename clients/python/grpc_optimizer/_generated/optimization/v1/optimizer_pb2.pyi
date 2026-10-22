from google.protobuf.internal import containers as _containers
from google.protobuf.internal import enum_type_wrapper as _enum_type_wrapper
from google.protobuf import descriptor as _descriptor
from google.protobuf import message as _message
from collections.abc import Iterable as _Iterable, Mapping as _Mapping
from typing import ClassVar as _ClassVar, Optional as _Optional, Union as _Union

DESCRIPTOR: _descriptor.FileDescriptor

class ObjectiveSense(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    OBJECTIVE_SENSE_UNSPECIFIED: _ClassVar[ObjectiveSense]
    OBJECTIVE_SENSE_MINIMIZE: _ClassVar[ObjectiveSense]
    OBJECTIVE_SENSE_MAXIMIZE: _ClassVar[ObjectiveSense]

class SolveStatus(int, metaclass=_enum_type_wrapper.EnumTypeWrapper):
    __slots__ = ()
    SOLVE_STATUS_UNSPECIFIED: _ClassVar[SolveStatus]
    SOLVE_STATUS_OPTIMAL: _ClassVar[SolveStatus]
    SOLVE_STATUS_INFEASIBLE: _ClassVar[SolveStatus]
    SOLVE_STATUS_UNBOUNDED: _ClassVar[SolveStatus]
    SOLVE_STATUS_INFEASIBLE_OR_UNBOUNDED: _ClassVar[SolveStatus]
    SOLVE_STATUS_TIME_LIMIT: _ClassVar[SolveStatus]
    SOLVE_STATUS_ITERATION_LIMIT: _ClassVar[SolveStatus]
    SOLVE_STATUS_INTERRUPTED: _ClassVar[SolveStatus]
    SOLVE_STATUS_OTHER_LIMIT: _ClassVar[SolveStatus]
    SOLVE_STATUS_UNKNOWN: _ClassVar[SolveStatus]
OBJECTIVE_SENSE_UNSPECIFIED: ObjectiveSense
OBJECTIVE_SENSE_MINIMIZE: ObjectiveSense
OBJECTIVE_SENSE_MAXIMIZE: ObjectiveSense
SOLVE_STATUS_UNSPECIFIED: SolveStatus
SOLVE_STATUS_OPTIMAL: SolveStatus
SOLVE_STATUS_INFEASIBLE: SolveStatus
SOLVE_STATUS_UNBOUNDED: SolveStatus
SOLVE_STATUS_INFEASIBLE_OR_UNBOUNDED: SolveStatus
SOLVE_STATUS_TIME_LIMIT: SolveStatus
SOLVE_STATUS_ITERATION_LIMIT: SolveStatus
SOLVE_STATUS_INTERRUPTED: SolveStatus
SOLVE_STATUS_OTHER_LIMIT: SolveStatus
SOLVE_STATUS_UNKNOWN: SolveStatus

class Variable(_message.Message):
    __slots__ = ("objective", "lower", "upper")
    OBJECTIVE_FIELD_NUMBER: _ClassVar[int]
    LOWER_FIELD_NUMBER: _ClassVar[int]
    UPPER_FIELD_NUMBER: _ClassVar[int]
    objective: float
    lower: float
    upper: float
    def __init__(self, objective: _Optional[float] = ..., lower: _Optional[float] = ..., upper: _Optional[float] = ...) -> None: ...

class Constraint(_message.Message):
    __slots__ = ("indices", "coefficients", "lower", "upper")
    INDICES_FIELD_NUMBER: _ClassVar[int]
    COEFFICIENTS_FIELD_NUMBER: _ClassVar[int]
    LOWER_FIELD_NUMBER: _ClassVar[int]
    UPPER_FIELD_NUMBER: _ClassVar[int]
    indices: _containers.RepeatedScalarFieldContainer[int]
    coefficients: _containers.RepeatedScalarFieldContainer[float]
    lower: float
    upper: float
    def __init__(self, indices: _Optional[_Iterable[int]] = ..., coefficients: _Optional[_Iterable[float]] = ..., lower: _Optional[float] = ..., upper: _Optional[float] = ...) -> None: ...

class LinearProgram(_message.Message):
    __slots__ = ("sense", "variables", "constraints")
    SENSE_FIELD_NUMBER: _ClassVar[int]
    VARIABLES_FIELD_NUMBER: _ClassVar[int]
    CONSTRAINTS_FIELD_NUMBER: _ClassVar[int]
    sense: ObjectiveSense
    variables: _containers.RepeatedCompositeFieldContainer[Variable]
    constraints: _containers.RepeatedCompositeFieldContainer[Constraint]
    def __init__(self, sense: _Optional[_Union[ObjectiveSense, str]] = ..., variables: _Optional[_Iterable[_Union[Variable, _Mapping]]] = ..., constraints: _Optional[_Iterable[_Union[Constraint, _Mapping]]] = ...) -> None: ...

class SolveRequest(_message.Message):
    __slots__ = ("request_id", "model", "diagnostics", "time_limit_seconds")
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    MODEL_FIELD_NUMBER: _ClassVar[int]
    DIAGNOSTICS_FIELD_NUMBER: _ClassVar[int]
    TIME_LIMIT_SECONDS_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    model: LinearProgram
    diagnostics: bool
    time_limit_seconds: float
    def __init__(self, request_id: _Optional[str] = ..., model: _Optional[_Union[LinearProgram, _Mapping]] = ..., diagnostics: _Optional[bool] = ..., time_limit_seconds: _Optional[float] = ...) -> None: ...

class SessionRequest(_message.Message):
    __slots__ = ("model", "input")
    MODEL_FIELD_NUMBER: _ClassVar[int]
    INPUT_FIELD_NUMBER: _ClassVar[int]
    model: LinearProgram
    input: SolveInput
    def __init__(self, model: _Optional[_Union[LinearProgram, _Mapping]] = ..., input: _Optional[_Union[SolveInput, _Mapping]] = ...) -> None: ...

class BoundOverride(_message.Message):
    __slots__ = ("index", "lower", "upper")
    INDEX_FIELD_NUMBER: _ClassVar[int]
    LOWER_FIELD_NUMBER: _ClassVar[int]
    UPPER_FIELD_NUMBER: _ClassVar[int]
    index: int
    lower: float
    upper: float
    def __init__(self, index: _Optional[int] = ..., lower: _Optional[float] = ..., upper: _Optional[float] = ...) -> None: ...

class SolveInput(_message.Message):
    __slots__ = ("request_id", "diagnostics", "time_limit_seconds", "objective_indices", "objective_values", "variable_bounds", "constraint_bounds")
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    DIAGNOSTICS_FIELD_NUMBER: _ClassVar[int]
    TIME_LIMIT_SECONDS_FIELD_NUMBER: _ClassVar[int]
    OBJECTIVE_INDICES_FIELD_NUMBER: _ClassVar[int]
    OBJECTIVE_VALUES_FIELD_NUMBER: _ClassVar[int]
    VARIABLE_BOUNDS_FIELD_NUMBER: _ClassVar[int]
    CONSTRAINT_BOUNDS_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    diagnostics: bool
    time_limit_seconds: float
    objective_indices: _containers.RepeatedScalarFieldContainer[int]
    objective_values: _containers.RepeatedScalarFieldContainer[float]
    variable_bounds: _containers.RepeatedCompositeFieldContainer[BoundOverride]
    constraint_bounds: _containers.RepeatedCompositeFieldContainer[BoundOverride]
    def __init__(self, request_id: _Optional[str] = ..., diagnostics: _Optional[bool] = ..., time_limit_seconds: _Optional[float] = ..., objective_indices: _Optional[_Iterable[int]] = ..., objective_values: _Optional[_Iterable[float]] = ..., variable_bounds: _Optional[_Iterable[_Union[BoundOverride, _Mapping]]] = ..., constraint_bounds: _Optional[_Iterable[_Union[BoundOverride, _Mapping]]] = ...) -> None: ...

class Solution(_message.Message):
    __slots__ = ("objective", "variable_values", "constraint_activities", "dual")
    OBJECTIVE_FIELD_NUMBER: _ClassVar[int]
    VARIABLE_VALUES_FIELD_NUMBER: _ClassVar[int]
    CONSTRAINT_ACTIVITIES_FIELD_NUMBER: _ClassVar[int]
    DUAL_FIELD_NUMBER: _ClassVar[int]
    objective: float
    variable_values: _containers.RepeatedScalarFieldContainer[float]
    constraint_activities: _containers.RepeatedScalarFieldContainer[float]
    dual: DualSolution
    def __init__(self, objective: _Optional[float] = ..., variable_values: _Optional[_Iterable[float]] = ..., constraint_activities: _Optional[_Iterable[float]] = ..., dual: _Optional[_Union[DualSolution, _Mapping]] = ...) -> None: ...

class DualSolution(_message.Message):
    __slots__ = ("constraint_duals", "reduced_costs")
    CONSTRAINT_DUALS_FIELD_NUMBER: _ClassVar[int]
    REDUCED_COSTS_FIELD_NUMBER: _ClassVar[int]
    constraint_duals: _containers.RepeatedScalarFieldContainer[float]
    reduced_costs: _containers.RepeatedScalarFieldContainer[float]
    def __init__(self, constraint_duals: _Optional[_Iterable[float]] = ..., reduced_costs: _Optional[_Iterable[float]] = ...) -> None: ...

class Timings(_message.Message):
    __slots__ = ("validation_seconds", "build_seconds", "solve_seconds")
    VALIDATION_SECONDS_FIELD_NUMBER: _ClassVar[int]
    BUILD_SECONDS_FIELD_NUMBER: _ClassVar[int]
    SOLVE_SECONDS_FIELD_NUMBER: _ClassVar[int]
    validation_seconds: float
    build_seconds: float
    solve_seconds: float
    def __init__(self, validation_seconds: _Optional[float] = ..., build_seconds: _Optional[float] = ..., solve_seconds: _Optional[float] = ...) -> None: ...

class SolveResult(_message.Message):
    __slots__ = ("request_id", "status", "solution", "timings", "solver_status", "execution")
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    STATUS_FIELD_NUMBER: _ClassVar[int]
    SOLUTION_FIELD_NUMBER: _ClassVar[int]
    TIMINGS_FIELD_NUMBER: _ClassVar[int]
    SOLVER_STATUS_FIELD_NUMBER: _ClassVar[int]
    EXECUTION_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    status: SolveStatus
    solution: Solution
    timings: Timings
    solver_status: str
    execution: ExecutionSettings
    def __init__(self, request_id: _Optional[str] = ..., status: _Optional[_Union[SolveStatus, str]] = ..., solution: _Optional[_Union[Solution, _Mapping]] = ..., timings: _Optional[_Union[Timings, _Mapping]] = ..., solver_status: _Optional[str] = ..., execution: _Optional[_Union[ExecutionSettings, _Mapping]] = ...) -> None: ...

class ExecutionSettings(_message.Message):
    __slots__ = ("cpu_budget", "solver_threads", "max_concurrent_solves")
    CPU_BUDGET_FIELD_NUMBER: _ClassVar[int]
    SOLVER_THREADS_FIELD_NUMBER: _ClassVar[int]
    MAX_CONCURRENT_SOLVES_FIELD_NUMBER: _ClassVar[int]
    cpu_budget: int
    solver_threads: int
    max_concurrent_solves: int
    def __init__(self, cpu_budget: _Optional[int] = ..., solver_threads: _Optional[int] = ..., max_concurrent_solves: _Optional[int] = ...) -> None: ...

class JobError(_message.Message):
    __slots__ = ("code", "message")
    CODE_FIELD_NUMBER: _ClassVar[int]
    MESSAGE_FIELD_NUMBER: _ClassVar[int]
    code: int
    message: str
    def __init__(self, code: _Optional[int] = ..., message: _Optional[str] = ...) -> None: ...

class JobOutcome(_message.Message):
    __slots__ = ("request_id", "result", "error")
    REQUEST_ID_FIELD_NUMBER: _ClassVar[int]
    RESULT_FIELD_NUMBER: _ClassVar[int]
    ERROR_FIELD_NUMBER: _ClassVar[int]
    request_id: str
    result: SolveResult
    error: JobError
    def __init__(self, request_id: _Optional[str] = ..., result: _Optional[_Union[SolveResult, _Mapping]] = ..., error: _Optional[_Union[JobError, _Mapping]] = ...) -> None: ...
