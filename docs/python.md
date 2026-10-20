# Python SDK

Create a continuous linear Pyomo model, then call `solve(model)`:

```python
from grpc_optimizer import solve

result = solve(model, endpoint="127.0.0.1:50051")
if result.is_optimal:
    print(result.objective)
```

Run `just python-setup` to install the SDK in the repository environment. In a
separate Python 3.14 project, use `uv add /path/to/grpc-optimizer`. Alternatively,
run `just python-build` and install the resulting wheel. Generated protobuf
bindings ship inside the SDK; importing it needs no source checkout, `sys.path`
changes, compiler, or stub generation.

## Repeated solves and async applications

Use a context manager to reuse a channel and close it reliably:

```python
from grpc_optimizer import PyomoClient

with PyomoClient("server:50051") as client:
    first = client.solve(first_model)
    second = client.solve(second_model, timeout=60)
```

For a notebook or an application with an existing event loop:

```python
from grpc_optimizer import solve_async, AsyncPyomoClient

result = await solve_async(model)

async with AsyncPyomoClient("server:50051") as client:
    first = await client.solve(first_model)
    second = await client.solve(second_model)
```

Create and use each async client within the same event loop. Keep a submitted
model unchanged until its solve completes. Independent models may share a client.
The synchronous API blocks the calling thread; use the async API in async code.

## Options and results

Both functions and both clients' `solve` methods accept these keyword options:

| Option | Default | Meaning |
| --- | --- | --- |
| `timeout` | `30` | Positive finite RPC deadline in seconds, including connection and queueing |
| `time_limit` | `None` | Optional positive finite solver time budget, subject to server limits |
| `diagnostics` | `False` | Request constraint activities, duals, and reduced costs |
| `request_id` | Generated UUID | Correlation ID, 1–128 UTF-8 bytes |
| `load_solution` | `True` | Load any returned feasible solution into the original model |

The one-call functions and client constructors accept `endpoint`. When omitted,
they use `OPT_GRPC_TARGET`, falling back to `127.0.0.1:50051`.

`SolveResult.status` is a `SolveStatus` enum, such as `OPTIMAL`, `INFEASIBLE`,
`UNBOUNDED`, or `TIME_LIMIT`. `is_optimal` indicates proven optimality.
`has_solution` indicates a feasible primal solution, and `solution_loaded`
indicates that it was written into Pyomo. A feasible solution returned at a limit
is loaded by default; it is not classified as optimal.

`objective` includes objective constants and fixed-variable contributions, even
when loading is disabled. It is `None` when no solution exists. With no solution,
previous model values remain unchanged; inspect the result before using them.
`raw` exposes the full protobuf response, including timings, execution settings,
and requested diagnostics. Constraint activities and dual arrays follow active
Pyomo constraint traversal order; reduced costs follow unfixed variable order.
Wire activities omit expression constants; the raw objective omits its offset.

Invalid or unsupported models and invalid options raise `ValueError` before any
RPC is sent. RPC failures propagate as `grpc.RpcError` (`grpc.aio.AioRpcError` in
async code), preserving status codes and details. Infeasibility and solver limits
are ordinary results. Async cancellation propagates to the gRPC call.
Malformed responses raise `ProtocolError`; the response ID, status, vector length,
and finite values are checked before any variables are updated.

The SDK supports one active linear objective, continuous variables, indexed and
nested components, fixed-variable substitution, and equality/ranged constraints.
Integer/binary, nonlinear, and unsupported active components are rejected.

## TLS and channel ownership

Default connections use plaintext gRPC. Supply a configured channel for TLS or
custom transport options:

```python
import grpc
from grpc_optimizer import PyomoClient

with grpc.secure_channel("server:443", grpc.ssl_channel_credentials()) as channel:
    with PyomoClient(channel=channel) as client:
        result = client.solve(model)
```

`AsyncPyomoClient(channel=channel)` accepts a `grpc.aio` channel. The caller owns
injected channels; closing the client leaves them open. Clients close channels
they create themselves. Supply either an endpoint or a channel.

## Architecture and maintenance

The public API is exported from `grpc_optimizer.__init__`. `adapter.py` converts
Pyomo expressions into the existing sparse LP contract and captures variable
order and objective offsets. `client.py` owns transport and channel lifecycle.
`result.py` validates responses and applies solutions. The server and protobuf
contract remain generic and independent of Pyomo.

The original `pyomo_adapter.to_solve_request` import remains available in the
repository as a compatibility shim. New low-level users can import it from
`grpc_optimizer.adapter`.

After changing the wire contract, run `just python-proto` and commit the generated
bindings. CI checks regeneration, static analysis, unit tests, distribution
building/imports, and interoperability with the Rust server. Run
`just python-check` locally, then `just python-test` with a running server.
