# grpc-optimizer

A stateless gRPC service for continuous linear programming, powered by HiGHS.
It accepts sparse LP models and supports unary, server-streaming, client-streaming,
and bidirectional calls. Includes Rust and Python clients, a Pyomo adapter,
example models, and a benchmark CLI.

- [API guide](docs/api.md): models, RPC modes, results, diagnostics, and errors.
- [Operations guide](docs/operations.md): configuration, resource limits,
  cancellation, deployment, and benchmarking.
- [Protobuf contract](proto/optimization/v1/optimizer.proto): message and service definitions.

## Quick start

Install `just`, Rust 1.88+, a C++17 compiler, CMake, libclang, and `protoc`.
On Debian/Ubuntu, install the native build dependencies with:

```sh
sudo apt-get install build-essential cmake libclang-dev protobuf-compiler
```

HiGHS builds from bundled sources; no separate solver installation is needed.
From the repository root, build and start the server:

```sh
just build
just run
```

In another terminal, check health and solve the production example:

```sh
just client --health
just client --model production --jobs 1 --diagnostics
```

The health check prints `SERVING`. The solve returns JSON with optimal status,
variable values `[60, 40]`, and objective `3600`. Stop the server with Ctrl-C.

## Rust client

The client defaults to unary calls, the production model, and three jobs.
Each job has a unique request ID. Use `--mode` to try the streaming modes:

```sh
just client --mode server-stream --model transportation
just client --mode client-stream --model blending
just client --mode bidi --model production --invalid-first
```

`--invalid-first` demonstrates a per-job error while the remaining jobs continue.
Unary RPC errors exit with a nonzero status; streaming modes print per-job errors
and continue. Solve output is JSON on stdout, with error messages on stderr.

Use `--deadline 5` for a five-second RPC deadline. It covers the whole call,
including every message in a stream. Run `just client --help` for all options.

## Python client and Pyomo

Install [uv](https://docs.astral.sh/uv/getting-started/installation/), then run
setup to create a Python 3.14 environment, install locked dependencies, and generate
protobuf and type stubs. uv downloads Python 3.14 if needed. Keep the server
running in another terminal.

```sh
just python-setup
just python-client --mode bidi --diagnostics
just python-client --mode client-stream --fixture examples/models/transportation.json
just python-client --mode server-stream --invalid-first
```

The Python client supports the same four modes as Rust. Both clients reuse a
channel across jobs and send and receive concurrently in bidirectional mode.
They close the input stream after submission and read all remaining results.

Python 3.14 is selected by `.python-version` and required by `pyproject.toml`.
Dependencies and development tools are recorded in `uv.lock`; setup and client
commands use `--locked` to prevent unexpected dependency changes. After editing
dependencies, run `uv lock`, then repeat setup. Repeat setup after changing the
protobuf contract as well.

Run `just python-fmt` to format and fix lint issues with Ruff, and
`just python-check` to check formatting, lint, and types with Ruff and Pyrefly,
then run converter tests without a server. Generated stubs are excluded from
formatting and linting; Pyrefly uses their types when checking the client code.

### Solve a Pyomo model

After Python setup, run:

```sh
just python-pyomo-client
```

For a remote server, use
`just python-pyomo-client --endpoint server:50051 --deadline 30`.
The [example](clients/python/pyomo_client.py) builds the production model and prints
optimal status, `x = 60`, `y = 40`, and objective `3600`.

To reuse [the converter](clients/python/pyomo_adapter.py), run from
`clients/python` and use your constructed `model` and connected `OptimizerStub`
as shown in the example:

```python
from pyomo_adapter import to_solve_request

request, variables = to_solve_request(model, request_id="my-model")
result = await stub.Solve(request, timeout=30)
if result.HasField("solution"):
    for var, value in zip(variables, result.solution.variable_values, strict=True):
        var.set_value(value)
```

Check `result.status` before treating the solution as optimal. The converter
supports continuous linear models with one active objective, indexed components,
fixed variables, and equality or ranged constraints. It substitutes fixed
variables and requires at least one unfixed variable. Integer, binary, nonlinear,
and unsupported components raise `ValueError`.

Objective constants stay in Pyomo because the API has no objective offset.
After loading the solution, evaluate the original objective with `pyo.value(...)`
to include its constant; the server's objective value excludes it.

## Example models

Both clients read fixtures from [examples/models](examples/models). Descriptions
explain units and variable order; fixtures may include expected answers.

| Model | Formulation | Optimum |
| --- | --- | --- |
| Production | Max `40x+30y`; `x+y≤100`, `2x+y≤160`, `x,y≥0` | `(60,40)`, profit `3600` |
| Transportation | Supplies `[20,30]`, demands `[10,15,25]`, costs `[[2,4,6],[3,1,2]]` | Ship `[[10,10,0],[0,5,25]]`, cost `115` |
| Blending | Min `4a+2b`; `a+b=100`, `.3a+.1b≥20`, `a,b≥0` | `(50,50)`, cost `300` |

Infeasible and unbounded fixtures demonstrate non-optimal statuses.
Generate a larger feasible model or export one for the Python client:

```sh
just client --generate production --size 10000 --jobs 1
just client --generate transportation --size 100 --export /tmp/transportation.json
just python-client --fixture /tmp/transportation.json --jobs 1
```

Export does not require a server. Production size is the number of products,
with up to four resource coefficients each. Transportation size is the number
of sources and destinations, giving `size²` routes and `2×size²` matrix entries.
Both generators use seeded SplitMix64 sequences.

## Configuration

Optionally copy [.env.example](.env.example) to `.env`. Just loads it automatically;
existing shell variables take precedence. Directly launched binaries read the
environment only.

| Setting | Default | Used by |
| --- | --- | --- |
| `OPT_LISTEN_ADDR` | `127.0.0.1:50051` | Server |
| `OPT_ENDPOINT` | `http://127.0.0.1:50051` | Rust client and benchmark |
| `OPT_GRPC_TARGET` | `127.0.0.1:50051` | Python clients |

Rust endpoints use an HTTP URI; Python endpoints use a gRPC `host:port` target.
See the [operations guide](docs/operations.md#configuration-and-limits) for all
server limits and CPU settings.

## Run with Docker

With Docker installed, build the image and start a container:

```sh
just docker-build
just docker-run -d -e OPT_CPU_BUDGET=2 -e OPT_MAX_CONCURRENT_SOLVES=2
just client --health
just docker-stop
```

Stop any local server using port 50051 first. The image and container default to
`grpc-optimizer`, with port publishing set to `127.0.0.1:50051:50051`.
Override these with `OPT_DOCKER_IMAGE`, `OPT_DOCKER_CONTAINER`, and
`OPT_DOCKER_PUBLISH` in your shell or `.env`.

Docker recipes forward build/run options. Pass server settings explicitly with
`-e NAME=value` or `--env-file`; Just's `.env` values are not automatically passed
into the container. With an env file copied from `.env.example`, also pass
`-e OPT_LISTEN_ADDR=0.0.0.0:50051` to listen on the container interface.
See [deployment notes](docs/operations.md#deployment) for health checks and ingress.

## Development commands

Run `just` to list all recipes. Rust build/run recipes use release builds and
Cargo's lockfile. Pass client, benchmark, and test arguments directly to Just:

| Command | Purpose |
| --- | --- |
| `just build` / `just run` | Build release binaries / start the server |
| `just client --help` / `just bench --help` | Show client / benchmark options |
| `just fmt` | Format Rust source |
| `just lint` / `just test` | Check formatting and Clippy / run Rust tests |
| `just check` | Run lint and Rust tests |
| `just python-setup` | Sync the locked Python 3.14 environment and generate stubs |
| `just python-client --help` / `just python-pyomo-client --help` | Show Python client options |
| `just python-fmt` / `just python-lint` | Format/fix with Ruff / check Ruff and Pyrefly |
| `just python-unit` / `just python-check` | Run converter tests / static checks and converter tests |
| `just python-test` | Run Pyomo converter tests and Python interoperability checks |
| `just docker-build` / `just docker-run` / `just docker-stop` | Build, run, and stop the container |

To verify changes:

```sh
just check
just build
just python-setup
just python-check
just python-test
```

Python checks require `just python-setup`; `just python-test` also needs a running
server. Select Rust tests with `just test --test models` or
`just test --test grpc -- --nocapture`.
CI runs formatting, Clippy, Rust tests, Ruff, Pyrefly, and Python 3.14
interoperability, including the Pyomo example. Tests cover known optima,
diagnostics, validation, all RPC modes, limits, backpressure, deadlines,
cancellation, and shutdown.

For performance comparisons, see [benchmarking](docs/operations.md#benchmarking).
