"""Build a Pyomo production model and solve it through the gRPC service."""

import sys

import click
import grpc
import pyomo.environ as pyo

from grpc_optimizer import solve


def build_model() -> pyo.ConcreteModel:
    """Build a two-product profit LP constrained by machine and material capacity."""
    model = pyo.ConcreteModel()
    model.x = pyo.Var(domain=pyo.NonNegativeReals)
    model.y = pyo.Var(domain=pyo.NonNegativeReals)
    model.profit = pyo.Objective(expr=40 * model.x + 30 * model.y, sense=pyo.maximize)
    model.machine = pyo.Constraint(expr=model.x + model.y <= 100)  # Machine capacity
    model.material = pyo.Constraint(expr=2 * model.x + model.y <= 160)  # Material capacity
    return model


def run(endpoint: str, deadline: float, access_token: str | None, tls: bool) -> None:
    """Solve the example model and print its status and any loaded solution."""
    model = build_model()
    result = solve(model, endpoint=endpoint, timeout=deadline, access_token=access_token, tls=tls)
    print(f"Status: {result.status.name}")
    if result.solution_loaded:
        print(f"x = {pyo.value(model.x):g}")
        print(f"y = {pyo.value(model.y):g}")
        print(f"Objective: {result.objective:g}")


@click.command(help=__doc__)
@click.option("--endpoint", default="127.0.0.1:50051", envvar="OPT_GRPC_TARGET")
@click.option("--access-token", envvar="OPT_ACCESS_TOKEN", help="IAM access token (or OPT_ACCESS_TOKEN)")
@click.option("--tls", is_flag=True, help="use TLS with trusted server certificates")
@click.option("--deadline", type=float, default=30)
def main(endpoint: str, access_token: str | None, tls: bool, deadline: float) -> None:
    """Parse connection options, validate the deadline, and report RPC failures."""
    if not 0 < deadline <= 86400:
        raise click.BadParameter("must be in (0,86400] seconds", param_hint="--deadline")
    try:
        run(endpoint, deadline, access_token, tls)
    except grpc.RpcError as error:
        print(f"RPC failed: {error.code().name}: {error.details()}", file=sys.stderr)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
