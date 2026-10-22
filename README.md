# grpc-optimizer

A gRPC service that solves continuous linear programs (LPs) with
[HiGHS](https://highs.dev). Send a model, get the solution back. The server keeps
no state, so you can run as many copies as you need.

The Python SDK takes [Pyomo](https://www.pyomo.org) models, sends them to the
server, and writes the solution back into the model.

More detail:

- [Python SDK guide](docs/python.md): all options, async usage, TLS.
- [API guide](docs/api.md): RPCs, results, diagnostics, errors.
- [Operations guide](docs/operations.md): limits, deployment, benchmarking.
- [Protobuf contract](proto/optimization/v1/optimizer.proto).

## 1. Host the server

### With Docker

No Rust toolchain needed. Images support `amd64` and `arm64`:

```sh
docker run -d --name grpc-optimizer --restart unless-stopped \
  -p 127.0.0.1:50051:50051 \
  ghcr.io/i1mercep/grpc-optimizer:latest
```

The server now listens on `127.0.0.1:50051`. Stop it with
`docker stop grpc-optimizer`.

Or use [compose.yaml](compose.yaml), which adds a health check and reads
settings from `.env`:

```sh
cp .env.example .env   # optional: edit settings
docker compose up -d
docker compose ps      # wait for (healthy)
```

Image tags: `latest` and `X.Y.Z` for releases, `edge` for the latest `main`
commit, `sha-<commit>` for any commit. Pin a version in production.

The server speaks plaintext gRPC with no authentication. Before exposing it
beyond localhost, put it on a private network or behind a TLS-terminating proxy.
See [deployment notes](docs/operations.md#deployment).

### From source

Install `just`, Rust 1.88+, a C++17 compiler, CMake, libclang, and `protoc`.
On Debian/Ubuntu:

```sh
sudo apt-get install build-essential cmake libclang-dev protobuf-compiler
```

HiGHS is built from bundled sources. Start the server:

```sh
just run               # builds, then listens on 127.0.0.1:50051; Ctrl-C to stop
just client --health   # in another terminal; prints SERVING
```

To build the Docker image yourself: `just docker-build`, `just docker-run -d`,
`just docker-stop`.

### Settings

Configure the server with environment variables: `-e NAME=value` for
`docker run`, or a `.env` file (copy [.env.example](.env.example)) for Compose
and `just`.

| Variable | Default | Meaning |
| --- | --- | --- |
| `OPT_LISTEN_ADDR` | `127.0.0.1:50051` (`0.0.0.0:50051` in Docker) | Listen address |
| `OPT_CPU_BUDGET` | Available CPUs | CPUs used for solving |
| `OPT_MAX_CONCURRENT_SOLVES` | CPU budget / solver threads | Simultaneous solves |
| `OPT_DEFAULT_TIME_LIMIT_SECONDS` | `30` | Solver time when the client sets none |
| `OPT_MAX_TIME_LIMIT_SECONDS` | `300` | Maximum solver time |
| `OPT_STREAM_WINDOW` | `8` | Unanswered jobs allowed per stream |

All limits are listed in the
[operations guide](docs/operations.md#configuration-and-limits).

## 2. Use the Python client

### Install

Install [uv](https://docs.astral.sh/uv/getting-started/installation/). The SDK
needs Python 3.14; uv downloads it if needed.

```sh
just python-setup                    # inside this repository
uv add /path/to/grpc-optimizer       # or: in your own project
```

The package includes the generated gRPC code, so users need neither Rust nor
`protoc`. To build a wheel instead, run `just python-build` and install the file
from `dist/`.

### Solve one Pyomo model

```python
import pyomo.environ as pyo
from grpc_optimizer import solve

model = pyo.ConcreteModel()
model.x = pyo.Var(domain=pyo.NonNegativeReals)
model.cost = pyo.Objective(expr=3 * model.x + 7)
model.demand = pyo.Constraint(expr=model.x >= 2)

result = solve(model)  # endpoint="host:port", else OPT_GRPC_TARGET, else 127.0.0.1:50051
if result.is_optimal:
    print(pyo.value(model.x))  # 2.0, loaded back into the model
    print(result.objective)    # 13.0, includes the constant
```

- Check `result.status` or `result.is_optimal` before using the values. If there
  is no solution, `result.objective` is `None` and the model is left unchanged.
- Options: `timeout=30` (whole-call deadline, seconds), `time_limit=` (solver
  budget), `load_solution=False` (leave the model untouched).
- Supported: continuous linear models with one objective, indexed components,
  fixed variables, and equality or ranged constraints. Integer, binary, and
  nonlinear models raise `ValueError`.

Run a complete example with `just python-pyomo-client`
([source](clients/python/pyomo_client.py)).

### Re-solve one model as data changes

Create a `GrpcOptimizer` with the model, then call `solve()` in a loop. The
model structure is sent once; each solve sends only the values that changed.
Change data through mutable Pyomo `Param`s:

```python
import pyomo.environ as pyo
from grpc_optimizer import GrpcOptimizer

model = pyo.ConcreteModel()
model.demand = pyo.Param(initialize=0, mutable=True)
model.x = pyo.Var(domain=pyo.NonNegativeReals)
model.cost = pyo.Objective(expr=3 * model.x + 7)
model.meet = pyo.Constraint(expr=model.x >= model.demand)

with GrpcOptimizer(model, "127.0.0.1:50051") as optimizer:
    for demand in (2, 5, 10):
        model.demand = demand
        result = optimizer.solve()  # sends only the new bound of model.meet
        print(demand, result.status.name, result.objective)
```

Output:

```text
2 OPTIMAL 13.0
5 OPTIMAL 22.0
10 OPTIMAL 37.0
```

- Objective coefficients, variable bounds, and constraint bounds may change.
  Other changes raise `ValueError`; create a new `GrpcOptimizer`.
- The optimizer keeps one call open on the server; use `with` or `close()`.
- In async code, use `AsyncGrpcOptimizer` and `await optimizer.solve()`.

See [docs/python.md](docs/python.md#re-solve-one-model-grpcoptimizer) and the
[wire protocol](docs/api.md#stream-inputs-for-one-model).

### Solve many different models

Reuse one connection with `PyomoClient`:

```python
from grpc_optimizer import PyomoClient

with PyomoClient("127.0.0.1:50051") as client:
    for model in models:
        result = client.solve(model)
```

To solve them in parallel, use `AsyncPyomoClient`; concurrent calls share one
connection:

```python
import asyncio
from grpc_optimizer import AsyncPyomoClient


async def solve_all(models):
    async with AsyncPyomoClient("127.0.0.1:50051") as client:
        return await asyncio.gather(*(client.solve(model) for model in models))
```

For the raw bidirectional `SolveStream` RPC, see the [API guide](docs/api.md)
and the stub-level example in [clients/python/client.py](clients/python/client.py).

## Example clients

Command-line clients in Rust and Python try the RPC modes (`unary`, `bidi`;
Rust also `session`) with the models in [examples/models](examples/models):

```sh
just client --model production --jobs 1               # Rust; optimum x=60, y=40, objective 3600
just client --mode session --jobs 3                   # Rust; model sent once
just python-client --mode bidi --diagnostics          # Python
just python-client --fixture examples/models/transportation.json
```

Add `--help` for all options. Rust clients use `OPT_ENDPOINT`
(`http://127.0.0.1:50051`); Python clients use `OPT_GRPC_TARGET`
(`127.0.0.1:50051`).

## Development

Run `just` to list all commands. Common ones:

| Command | Purpose |
| --- | --- |
| `just check` | Rust lint and tests |
| `just python-check` | Python lint, types, and unit tests (no server needed) |
| `just python-test` | Python unit and interoperability tests (needs a running server) |
| `just python-fmt` / `just fmt` | Format Python / Rust |
| `just python-proto` | Regenerate Python bindings after changing the `.proto` file |
| `just bench --help` | Benchmark options; see [benchmarking](docs/operations.md#benchmarking) |
