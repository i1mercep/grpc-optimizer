# API guide

[Back to README](../README.md) · [Operations guide](operations.md)

The [protobuf contract](../proto/optimization/v1/optimizer.proto) defines package
`optimization.v1` and service `Optimizer`. Each job contains a complete,
independent continuous LP model.

## Choose a call pattern

| CLI mode | RPC | Input | Output |
| --- | --- | --- | --- |
| `unary` | `Solve` | One LP | One result |
| `server-stream` | `SolveBatch` | Bounded batch of LPs | Results in completion order |
| `client-stream` | `Collect` | Bounded stream of LPs | One response in submission order |
| `bidi` | `SolveStream` | Open stream of LPs | Results in completion order |

Use unary for simple request/response, server streaming for early results from a
finite batch, client streaming for incremental submission with one final response,
and bidirectional streaming for ongoing submission and early results.

All modes reuse HTTP/2 connections; concurrent unary calls can also share one
connection. Streaming sends independent models, not chunks of one model. Choose
the behavior you need and benchmark your workload before assuming a speed gain.

## Build a model

A `SolveRequest` contains a `request_id`, a `LinearProgram`, an optional
`time_limit_seconds`, and a `diagnostics` flag. The LP contains an objective sense,
continuous variables, and sparse constraint rows.

- Set the sense to minimize (`1`) or maximize (`2`). Unspecified sense is invalid.
- Each variable has an objective coefficient and optional lower/upper bounds.
- Each constraint has parallel `indices` and `coefficients` arrays of equal length.
  Indices are zero-based and must not repeat within a row.
- Omitted lower and upper bounds mean negative and positive infinity. Set
  `lower: 0` explicitly for nonnegative variables.
- Set equal lower and upper bounds for an equality constraint.
- Coefficients and explicit bounds must be finite; lower bounds must not exceed
  upper bounds. A model must contain at least one variable.

The shared [JSON fixtures](../examples/models) use protobuf field names and numeric
objective senses. Each fixture wraps the solve request in a `request` field and
may also contain a description and expected answers.

Supply request IDs of 1–128 UTF-8 bytes, unique within a call. The server echoes
IDs but does not track uniqueness across messages, deduplicate, or retry jobs.
Reusing an ID submits the work again.

## Read a result

RPC success means a result was delivered. Check `SolveResult.status` to determine
the optimization outcome: optimal, infeasible, unbounded, infeasible-or-unbounded,
time/iteration/other limit, interrupted, or unknown. `solver_status` provides the
native solver status text.

A `solution` exists only when HiGHS reports primal feasibility. It may be present
for an unbounded or limit-reached model, so its presence does not imply optimality.
Treat the objective as optimal only when the status is `OPTIMAL`.

With diagnostics enabled, the solution includes constraint activities and, when
HiGHS reports dual feasibility, a `dual` submessage. Dual signs follow HiGHS's
objective-sense convention; reduced costs satisfy `c - Aᵀy`. Degenerate optima
may have different valid dual solutions.

Every result includes execution settings (CPU budget, solver threads, and solve
concurrency) and timings for validation, native-model construction, and solving.
Timings exclude networking, encoding, result extraction, and executor scheduling.

## Handle errors and deadlines

| Failure | Canonical gRPC code |
| --- | --- |
| Invalid model or request | `INVALID_ARGUMENT` |
| Capacity or size limit | `RESOURCE_EXHAUSTED` |
| Native execution failure | `INTERNAL` |

`Solve` returns these as RPC errors. Other methods return per-job `JobError`
outcomes, correlated by request ID, and continue processing other jobs. A
`JobError.code` is the numeric canonical gRPC status code.

Transport errors, finite batch overflow, collected response overflow, deadlines,
and shutdown terminate the whole RPC. `Collect` returns no partial response.

A client deadline covers the entire RPC, including all messages in a stream.
Tonic may report an expired call as `CANCELLED` when its transport timeout arrives
before the handler's `DEADLINE_EXCEEDED` response. Both terminate the call.

A solver time budget produces a `TIME_LIMIT` solve status; RPC cancellation or
deadline expiry produces a transport error. See
[time limits and cancellation](operations.md#time-limits-and-cancellation) for
cooperative interruption and cleanup behavior.
