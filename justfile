set dotenv-load
set positional-arguments

image := env("OPT_DOCKER_IMAGE", "grpc-optimizer")
container := env("OPT_DOCKER_CONTAINER", "grpc-optimizer")
publish := env("OPT_DOCKER_PUBLISH", "127.0.0.1:50051:50051")

# List available commands.
default:
    @just --list

# Build all release binaries.
build:
    cargo build --locked --release

# Run the release server (Ctrl-C to stop).
run:
    cargo run --locked --release --bin grpc-optimizer-server

# Run the Rust client; forward CLI options unchanged.
client *args:
    cargo run --quiet --locked --release --bin grpc-optimizer-client -- "$@"

# Benchmark direct and gRPC solving; forward CLI options unchanged.
bench *args:
    cargo run --quiet --locked --release --bin grpc-optimizer-bench -- "$@"

# Format Rust source.
fmt:
    cargo fmt --all

# Check Rust formatting and run Clippy with warnings denied.
lint:
    cargo fmt --all -- --check
    cargo clippy --locked --all-targets -- -D warnings

# Run Rust unit and loopback integration tests; optionally pass Cargo test arguments.
test *args:
    cargo test --locked "$@"

# Run all local Rust checks.
check: lint test

# Sync the locked Python 3.14 environment and generate gRPC/type stubs.
python-setup:
    uv sync --locked
    mkdir -p clients/python/generated
    uv run --locked python -m grpc_tools.protoc -I proto --python_out=clients/python/generated --pyi_out=clients/python/generated --grpc_python_out=clients/python/generated proto/optimization/v1/optimizer.proto

# Run the Python client (requires python-setup).
python-client *args:
    uv run --locked python clients/python/client.py "$@"

# Build a Pyomo model and solve it over gRPC (requires python-setup).
python-pyomo-client *args:
    uv run --locked python clients/python/pyomo_client.py "$@"

# Format Python clients and fix lint issues.
python-fmt:
    uv run --locked ruff check --fix clients/python
    uv run --locked ruff format clients/python

# Check Python formatting, lint, and types (requires python-setup).
python-lint:
    uv run --locked ruff format --check clients/python
    uv run --locked ruff check clients/python
    uv run --locked pyrefly check

# Run Python converter tests without a server (requires python-setup).
python-unit:
    uv run --locked python -m unittest discover -s clients/python -p 'test_*.py'

# Run Python static checks and converter tests without a server.
python-check: python-lint python-unit

# Test Pyomo conversion and Python interoperability (requires a running server).
python-test: python-unit
    uv run --locked python clients/python/smoke_test.py

# Build the local container image; optionally pass Docker build options.
docker-build *args:
    docker build --tag {{ quote(image) }} "$@" .

# Run the container; optionally pass Docker run options such as -d or -e NAME=value.
docker-run *args:
    docker run --rm --name {{ quote(container) }} --publish {{ quote(publish) }} "$@" {{ quote(image) }}

# Stop the container started by docker-run; --rm removes it after exit.
docker-stop:
    docker stop {{ quote(container) }}
