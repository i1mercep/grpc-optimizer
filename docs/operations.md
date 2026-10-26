# Operations guide

[Back to README](../README.md) · [API guide](api.md)

## Configuration and limits

Set server options through environment variables. [.env.example](../.env.example)
provides a starting point; Just loads an optional `.env`, with shell variables
taking precedence. Directly launched binaries do not load `.env`.

| Variable | Default | Purpose |
| --- | --- | --- |
| `OPT_LISTEN_ADDR` | `127.0.0.1:50051` | Listen address |
| `OPT_CPU_BUDGET` | Available CPUs | CPU budget for native solves |
| `OPT_SOLVER_THREADS` | `1` | HiGHS threads per solve |
| `OPT_MAX_CONCURRENT_SOLVES` | CPU budget / threads | Global simultaneous solves |
| `OPT_MAX_ACTIVE_RPCS` | `32` | Admitted optimization RPCs |
| `OPT_MAX_MESSAGE_BYTES` | `16777216` (16 MiB) | Protobuf message size limit |
| `OPT_STREAM_WINDOW` | `8` | Outstanding jobs per stream |
| `OPT_DEFAULT_TIME_LIMIT_SECONDS` | `30` | Default solver time budget |
| `OPT_MAX_TIME_LIMIT_SECONDS` | `300` | Maximum solver time budget |
| `OPT_MAX_VARIABLES` | `100000` | Variables per model |
| `OPT_MAX_CONSTRAINTS` | `100000` | Constraint rows per model |
| `OPT_MAX_NONZEROS` | `1000000` | Stored coefficients per model |

CPU settings must satisfy `concurrent solves × solver threads ≤ CPU budget`.
The default solve concurrency is the integer quotient of budget and threads.
All count and size limits must be positive. Time limits must satisfy
`0 < default ≤ maximum ≤ 300 seconds`.

The server rejects work immediately when global solve capacity is full. Retry
with backoff or reduce client concurrency. Start with one solver thread per job;
more threads do not guarantee faster LP solves. Compare fewer multithreaded jobs
with more single-threaded jobs, restarting the service between settings. Keep
thread settings constant during the process lifetime.

The stream window counts both running jobs and completed results waiting for the
client. Its effective size is also limited by global solve concurrency. When the
window fills, the server stops reading input until space is available. HTTP/2 and
client libraries have their own bounded buffers.

Model limits control admission, not native memory usage. Tune them and solve
concurrency to available memory. The RPC cap applies to application handlers;
it does not cap sockets or memory used before decoding. Use ingress connection
limits for external deployments.

## Time limits and cancellation

A request may set a positive `time_limit_seconds`. The server clamps it to the
configured maximum and the remaining RPC deadline after model construction.

The server checks cancellation before native solving and after result extraction.
During simplex and interior-point solving, interrupt callbacks check for client
disconnects and expired deadlines in both RPC modes. Terminating a stream
also cancels its unfinished jobs.

Interruption is cooperative. Presolve, factorization, and native work between
callbacks can delay it; callbacks and solver time limits do not impose a hard
execution cutoff. The execution permit remains held until the blocking worker
finishes native cleanup, even if the async caller has already disappeared.

On shutdown, the server marks health as not-serving, stops admissions and stream
input, and waits for admitted native work to finish. Jobs in terminated streams
receive cancellation; active unary solves can finish normally.

## Deployment

See the [Docker quick start](../README.md#with-docker) for image and container
commands. The runtime runs as a non-root user and includes a gRPC health probe
that follows the port in `OPT_LISTEN_ADDR`. The service supports standard gRPC
health checks for `optimization.v1.Optimizer` and v1 reflection.

The default CPU budget follows the container's CPU quota, so `--cpus` (or
Compose `cpus`) is usually enough. Shutdown waits for admitted solves, which can
run up to `OPT_MAX_TIME_LIMIT_SECONDS`; give `docker stop -t` or Kubernetes
`terminationGracePeriodSeconds` at least that long.

The [image workflow](../.github/workflows/docker.yml) builds `amd64` and `arm64`
natively, smoke-tests each (health, solve, graceful exit), and on `main` pushes or published
GitHub releases publishes a multi-platform manifest to GHCR with SBOM and provenance.
Pull requests build and test without publishing. GHCR packages start private;
make the package public in its GitHub settings for anonymous pulls.

Local defaults use loopback and plaintext. Use a private network or terminate TLS
and authentication at a gRPC-capable ingress. The service has no built-in
authentication.

Logs are JSON lines on stdout, written by a background thread; if stdout stalls,
lines are dropped rather than delaying solves. Per-solve timings are `debug`
events and are always returned in each result.

Replicas share no state. An established stream stays on one replica; distributing
load may require multiple channels or client/load-balancer support for routing
new RPCs. Results are not durable, and disconnected jobs cannot be resumed.

## Benchmarking

Run the release server in one terminal:

```sh
RUST_LOG=warn just run
```

In another terminal:

```sh
just bench --family production --size 100 --jobs 1000 --concurrency 1
just bench --family transportation --size 100 --jobs 20 --concurrency 1
```

The default concurrency is one, which works on a one-CPU host. Increase it only
with enough server capacity. For example, use `--concurrency 4` with at least four
available solve slots. The default mode compares direct solving, unary, and
bidirectional calls; use `just bench --help` for options.

Direct mode runs without a server:

```sh
just bench --mode direct --family production --size 1000
```

All modes solve identical seeded inputs:

| Mode | Execution and latency measurement |
| --- | --- |
| Direct | In-process validation/engine/solver path, without network serialization |
| Unary | Concurrent calls using cloned stubs on one channel |
| Bidirectional | One stream with a bounded outstanding window; latency starts once a slot is available |

A bidirectional deadline covers the entire measured stream.

Each JSON report includes dimensions, seed, concurrency, actual execution settings,
connection setup, per-mode warm-up, throughput, p50/p95/p99 job latency, server
validation/build/solve distributions, and failures. Failed or missing jobs are
counted. Latency percentiles include failures; server timings exist only for
returned solve results. Model preparation, setup, and warm-up are excluded from
the measured interval.

Use controlled hardware, record CPU/memory limits, and repeat trials. Direct and
remote runs compete with other host work; measurements do not establish a
universal latency or streaming speed advantage.

## Code layout

| Module | Responsibility |
| --- | --- |
| `model` | Validate input |
| `solver` | Build and solve HiGHS models |
| `engine` | Admit jobs and manage native work |
| `grpc` | Implement transport and streaming |
| `config` | Read and validate settings |
| `samples` | Load fixtures and generate models |

Generated protobuf types form the shared data contract. Each native solver
instance is created, solved, and destroyed in a blocking worker, while Tokio
handles network I/O. A session keeps its last instance for the next job. A
global semaphore is acquired before spawning native work to avoid an unbounded
blocking queue. The service needs no database or broker and uses one package
with one solver backend.

Lifecycle tests synchronize with simplex/interior-point callbacks before
cancelling or expiring solves. Loopback tests cover disconnects and deadlines in
both RPC modes, permit retention until native exit, and capacity recovery.
