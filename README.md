# grpc-optimizer

A stateless continuous linear programming service using Tokio, Tonic/Prost, and
HiGHS. One package, one solver backend, four gRPC call patterns. Includes Rust and
Python clients, shared example models, and a benchmark CLI.

## Run locally

Requires `just`, Rust 1.88+, a C++17 compiler, CMake, libclang, and `protoc`. HiGHS is built
from bundled native sources by `highs-sys`; no separately installed solver is needed.
On Debian/Ubuntu:

```sh
sudo apt-get install build-essential cmake libclang-dev protobuf-compiler
just build
just run
```

In another terminal:

```sh
just client --health
just client --mode unary --model production --diagnostics
just client --mode server-stream --model transportation
just client --mode client-stream --model blending
just client --mode bidi --model production --invalid-first
```

`--jobs` defaults to 3. Each job gets a unique request ID. Unary errors terminate
the CLI with a nonzero exit; other patterns print per-job errors and continue.
Use `--deadline 5` for an RPC deadline. It covers the entire call, including all
messages in a stream. Client output is JSON on stdout; errors go to stderr.
Tonic's transport may report an expired call as `CANCELLED` when its timeout wins
the race with the handler's `DEADLINE_EXCEEDED` response; both terminate the call.

Environment variables are listed in [.env.example](.env.example). Optionally copy
it to `.env`; Just loads that file automatically, with existing shell variables
taking precedence. Binaries run directly read environment variables only. Rust
clients accept `OPT_ENDPOINT` (an HTTP URI); Python uses `OPT_GRPC_TARGET` (a native
gRPC `host:port` target).

## Commands

Run `just` or `just --list` to see all recipes. Rust run commands use release
builds and Cargo's lockfile. Client, benchmark, and test recipes forward arguments
without requiring an extra `--` before the application options.

| Command | Purpose |
| --- | --- |
| `just build` / `just run` | Build release binaries / start the server |
| `just client --help` | Rust client options, including health checks and all four RPC modes |
| `just bench --help` | Benchmark options |
| `just fmt` | Format Rust source |
| `just lint` / `just test` | Check formatting and Clippy / run Rust tests |
| `just check` | Run lint and Rust tests |
| `just python-setup` | Create/update `.venv`, install dependencies, and generate stubs |
| `just python-client --help` / `just python-test` | Python client / interoperability checks |
| `just docker-build` / `just docker-run` / `just docker-stop` | Build, run, and stop the container |

Python clients and interoperability checks require `just python-setup` first.
Remote client examples, Python tests, and benchmarks other than `--mode direct`
require a running server. Run it in a separate terminal with `just run`.

## Python client

```sh
just python-setup
just python-client --mode bidi --diagnostics
just python-client --mode client-stream --fixture examples/models/transportation.json
just python-client --mode server-stream --invalid-first
just python-test
```

The Python stub is generated from the same `.proto` as Rust. Bidirectional clients
send and receive concurrently, then half-close input and drain remaining results.
Both clients reuse their channels across jobs.
Set `PYTHON` to choose the interpreter for setup, for example
`PYTHON=python3.12 just python-setup`. Repeat setup after changing the protobuf
contract or Python dependencies.

## Contract and call patterns

The public contract is [optimizer.proto](proto/optimization/v1/optimizer.proto),
package `optimization.v1`, service `Optimizer`.

| RPC | Input | Output | Useful for |
| --- | --- | --- | --- |
| `Solve` | One complete LP | One result | Simple request/response and concurrent unary calls |
| `SolveBatch` | Bounded batch of LPs | Results in completion order | Finite workloads with early results |
| `Collect` | Bounded stream of LPs | One response in submission order | Incremental submission with one final response |
| `SolveStream` | Open stream of independent LPs | Results in completion order | Continuous job submission and early responses |

These are RPC patterns on reusable HTTP/2 connections, not separate connection
protocols. Unary calls also multiplex on HTTP/2. Streaming is a behavior choice;
measure your workload before assuming a speed advantage. Client streaming sends
complete independent models, not chunks of one model.

A model specifies minimize/maximize, continuous variables with objective
coefficients and optional bounds, and sparse constraint rows. Each row has equal
length `indices` and `coefficients` arrays with zero-based variable indices.
Omitted bounds mean negative/positive infinity. Set `lower: 0` explicitly for
nonnegative variables. Equality constraints use equal lower and upper bounds.
All coefficients and explicit bounds must be finite. Duplicate indices in a row,
reversed bounds, empty variable lists, and unspecified senses are rejected.

Supply request IDs of 1–128 UTF-8 bytes, unique within a call. IDs are echoed;
uniqueness is the caller's responsibility and is not stored across messages.
The server does not deduplicate or retry jobs. Reusing an ID resubmits work.

`SolveResult.status` describes optimization, independently of RPC success. It
preserves infeasible, unbounded, infeasible-or-unbounded, time/iteration/other limits,
and interrupted outcomes. A `solution` is present only if HiGHS reports primal
feasibility; it may exist for an unbounded or limit-reached model and does **not**
imply optimality. Check status before treating its objective as optimal.

Diagnostics add constraint activities, and a dual submessage only when HiGHS
reports dual feasibility. Duals follow HiGHS's objective-sense sign convention;
reduced costs satisfy `c - Aᵀy`. Degenerate optima can have different valid duals.
Timings measure validation, native-model construction, and native solve wall time;
they exclude network, encoding, result extraction, and executor scheduling.
Execution settings report the server's CPU budget, threads, and solve concurrency.

Malformed unary inputs produce `INVALID_ARGUMENT`; capacity limits produce
`RESOURCE_EXHAUSTED`; native execution failures produce `INTERNAL`. Other methods
embed those canonical gRPC codes in correlated `JobError` outcomes and keep going.
Transport errors, finite batch overflow, collected response overflow, deadlines,
and shutdown terminate the whole RPC. No partial `Collect` response is returned.

## Architecture and resource limits

`model` validates plain input; `solver` constructs and solves a HiGHS model;
`engine` owns admission and native-work lifecycle; `grpc` implements transport and
streaming. `config` centralizes settings and `samples` loads/generates models.
Generated protobuf types are the shared data contract. No database, broker,
generic solver abstraction, or separate crate workspace is required.

Every native instance is created, solved, and destroyed within a blocking worker.
Tokio handles network I/O. A global semaphore is acquired before spawning native
work, avoiding a hidden unbounded blocking queue. Requests fail fast when global
solver capacity is occupied; retry with backoff or reduce client concurrency.

By default each job uses one HiGHS thread, and concurrent solves equal available
CPUs. Override `OPT_CPU_BUDGET`, `OPT_SOLVER_THREADS`, and
`OPT_MAX_CONCURRENT_SOLVES`; concurrency × threads must fit the CPU budget. Keep
thread settings constant for the lifetime of a process. More solver threads do
not guarantee faster LP solves. Benchmark fewer concurrent multithreaded jobs
against more single-threaded jobs, restarting the service between settings.

Defaults are 32 admitted optimization RPCs, 16 MiB protobuf messages, 16 MiB
collected responses, 64 jobs per finite batch, and 8 outstanding jobs per stream
(further limited by global solve concurrency). The per-stream window includes
completed results awaiting consumption. Backpressure stops reading inputs when
the window fills. HTTP/2 and client libraries also have their own bounded buffers.
The RPC admission cap applies to application handlers; it is not a global socket
or pre-decode memory cap. Set ingress connection limits for external deployments.

Individual models default to at most 100,000 variables, 100,000 rows, and 1,000,000
stored coefficients. These are admission limits, not native memory guarantees.
Tune them and the concurrency to the memory available to each instance.

The default solver budget is 30 seconds, with an absolute server maximum of 300.
Clients may request a positive `time_limit_seconds`, clamped to that maximum and
the remaining RPC deadline after construction. Validation and construction check
cancellation before starting native solve. Once HiGHS is running, disconnecting
cannot abort `spawn_blocking`: its execution permit remains held until it returns.
HiGHS time limits are best effort, not a hard execution cutoff. Shutdown marks
health not-serving, stops admissions/stream input, and drains running native work.

## Example models

Fixtures in [examples/models](examples/models) are read by both clients. JSON uses
the protobuf field names, numeric sense `1=minimize`, `2=maximize`, and optional
expected answers. Descriptions explain units and variable order.

| Model | Formulation | Optimum |
| --- | --- | --- |
| Production | Max `40x+30y`; `x+y≤100`, `2x+y≤160`, `x,y≥0` | `(60,40)`, profit `3600` |
| Transportation | Supplies `[20,30]`, demands `[10,15,25]`, costs `[[2,4,6],[3,1,2]]` | Ship `[[10,10,0],[0,5,25]]`, cost `115` |
| Blending | Min `4a+2b`; `a+b=100`, `.3a+.1b≥20`, `a,b≥0` | `(50,50)`, cost `300` |

Infeasible and unbounded fixtures demonstrate non-optimal statuses. Seeded
generators create feasible production/resource-allocation and transportation LPs:

```sh
just client --generate production --size 10000 --mode unary --jobs 1
just client --generate transportation --size 100 --export /tmp/transportation.json
just python-client --fixture /tmp/transportation.json --jobs 1
```

Production size means products, with up to four resource coefficients each;
transportation size means sources and destinations, giving `size²` routes and
`2×size²` matrix entries. The generators use a seeded SplitMix64 sequence.

## Measure performance

Run the release server separately. The benchmark defaults to concurrency 1 so it
also works on a one-CPU host. Increase concurrency together with server capacity.

```sh
RUST_LOG=warn just run
```

```sh
just bench --family production --size 100 --jobs 1000 --concurrency 4
just bench --family transportation --size 100 --jobs 20 --concurrency 1
just bench --mode direct --family production --size 1000
```

Each mode solves identical seeded inputs. Direct mode uses the same in-process
validation/engine/solver path without network serialization. Concurrent unary
uses cloned stubs sharing one channel. Server/client streaming use batches of up
to the chosen concurrency; bidi keeps one stream open with a bounded outstanding
window. Batch latency starts at batch submission; `Collect` latency includes
waiting for the complete batch. Bidi latency starts after an outstanding slot is
available. A bidi deadline covers its entire measured stream.

Each JSON report includes dimensions, seed, concurrency, actual execution settings,
connection setup, per-mode warm-up, throughput, p50/p95/p99 job latency, separate
server validation/build/solve distributions, and failures. Failed or missing jobs
are counted, not silently omitted. Percentiles include failures; server phase
timings exist only for returned solve results. Setup and warm-up are excluded from
the measured interval. Input models are prepared before the measured interval.

Direct and remote runs may compete with other host work. Run on controlled
hardware, record CPU/memory limits, and compare repeated trials. No universal
latency or streaming speed-up is claimed.

## Container deployment

```sh
just docker-build
just docker-run -d -e OPT_CPU_BUDGET=2 -e OPT_MAX_CONCURRENT_SOLVES=2
just client --health
just docker-stop
```

The image and container name default to `grpc-optimizer`; port publishing defaults
to `127.0.0.1:50051:50051`. Override them with `OPT_DOCKER_IMAGE`,
`OPT_DOCKER_CONTAINER`, and `OPT_DOCKER_PUBLISH` in your shell or `.env`.
Docker build/run options pass through to the corresponding command. Server
configuration must be passed explicitly with `-e NAME=value` or `--env-file`;
loading `.env` in Just does not forward all variables into the container. When
using an env file copied from `.env.example`, add `-e OPT_LISTEN_ADDR=0.0.0.0:50051`
so the service listens on the container network interface.

The runtime is non-root and includes a gRPC health probe. If overriding the listen
port, override the container health command too. The service implements standard
gRPC health checks for `optimization.v1.Optimizer` and v1 reflection.

Local defaults use loopback and plaintext. Deploy on a private network or terminate
TLS/authentication at a gRPC-capable ingress; built-in authentication and public
hosting are outside this version. Replicas have no shared state. Established
streams remain attached to one replica, so distributing load may require multiple
channels or client-side/load-balancer support for new RPCs. Results are not durable
and disconnected jobs cannot be resumed.

## Verify

```sh
just check
just build
just python-test
```

To select Rust tests, use e.g. `just test --test models` or
`just test --test grpc -- --nocapture`. Python checks need a running server.

Tests cover known optima, feasibility/objective/dual checks, seeded generation,
input validation, native limits, all RPC patterns on loopback HTTP/2, per-job
errors, backpressure, deadlines, finite limits, and cancellation/shutdown lifecycle.
CI runs formatting, Clippy, Rust tests, and Python interoperability.
