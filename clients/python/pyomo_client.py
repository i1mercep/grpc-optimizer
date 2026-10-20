"""Build a Pyomo production model and solve it through the gRPC service."""

import argparse
import os
import sys
from typing import Any

import grpc
import pyomo.environ as pyo

from grpc_optimizer import solve


def build_model() -> Any:
    """Build a two-product profit LP constrained by machine and material capacity."""
    model = pyo.ConcreteModel()
    model.x = pyo.Var(domain=pyo.NonNegativeReals)
    model.y = pyo.Var(domain=pyo.NonNegativeReals)
    model.profit = pyo.Objective(expr=40 * model.x + 30 * model.y, sense=pyo.maximize)
    model.machine = pyo.Constraint(expr=model.x + model.y <= 100)  # Machine capacity
    model.material = pyo.Constraint(expr=2 * model.x + model.y <= 160)  # Material capacity
    return model


def run(args: argparse.Namespace) -> None:
    """Solve the example model and print its status and any loaded solution."""
    model = build_model()
    result = solve(model, endpoint=args.endpoint, timeout=args.deadline)
    print(f"Status: {result.status.name}")
    if result.solution_loaded:
        print(f"x = {pyo.value(model.x):g}")
        print(f"y = {pyo.value(model.y):g}")
        print(f"Objective: {result.objective:g}")


def main() -> None:
    """Parse connection options, validate the deadline, and report RPC failures."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", default=os.environ.get("OPT_GRPC_TARGET", "127.0.0.1:50051"))
    parser.add_argument("--deadline", type=float, default=30)
    args = parser.parse_args()
    if not 0 < args.deadline <= 86400:
        parser.error("deadline must be in (0,86400] seconds")
    try:
        run(args)
    except grpc.RpcError as error:
        print(f"RPC failed: {error.code().name}: {error.details()}", file=sys.stderr)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
