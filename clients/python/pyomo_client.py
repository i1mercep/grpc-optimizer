"""Build a Pyomo production model and solve it through the gRPC service."""

import argparse
import asyncio
import os
import sys
from typing import Any

import pyomo.environ as pyo

from client import grpc, pb, rpc
from pyomo_adapter import to_solve_request


def build_model() -> Any:
    model = pyo.ConcreteModel()
    model.x = pyo.Var(domain=pyo.NonNegativeReals)
    model.y = pyo.Var(domain=pyo.NonNegativeReals)
    model.profit = pyo.Objective(expr=40 * model.x + 30 * model.y, sense=pyo.maximize)
    model.machine = pyo.Constraint(expr=model.x + model.y <= 100)
    model.material = pyo.Constraint(expr=2 * model.x + model.y <= 160)
    return model


async def run(args: argparse.Namespace) -> None:
    model = build_model()
    request, variables = to_solve_request(model, request_id="pyomo-production")
    options = [
        ("grpc.max_receive_message_length", 16 * 1024 * 1024),
        ("grpc.max_send_message_length", 16 * 1024 * 1024),
    ]
    async with grpc.aio.insecure_channel(args.endpoint, options=options) as channel:
        await asyncio.wait_for(channel.channel_ready(), timeout=5)
        result = await rpc.OptimizerStub(channel).Solve(request, timeout=args.deadline)
    print(f"Status: {pb.SolveStatus.Name(result.status)}")
    if result.HasField("solution"):
        for var, value in zip(variables, result.solution.variable_values, strict=True):
            var.set_value(value)
            print(f"{var.name} = {pyo.value(var):g}")
        print(f"Objective: {pyo.value(model.profit):g}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", default=os.environ.get("OPT_GRPC_TARGET", "127.0.0.1:50051"))
    parser.add_argument("--deadline", type=float, default=30)
    args = parser.parse_args()
    if not 0 < args.deadline <= 86400:
        parser.error("deadline must be in (0,86400] seconds")
    try:
        asyncio.run(run(args))
    except grpc.aio.AioRpcError as error:
        print(f"RPC failed: {error.code().name}: {error.details()}", file=sys.stderr)
        raise SystemExit(1) from error
    except TimeoutError as error:
        print("Timed out connecting to the gRPC server", file=sys.stderr)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
