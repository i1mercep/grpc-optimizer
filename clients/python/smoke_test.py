"""Interoperability checks against a running grpc-optimizer-server."""

import asyncio
import math
import os

import pyomo.environ as pyo

from client import MODES, ROOT, grpc, load_jobs, outcomes, pb, rpc
from grpc_optimizer import (
    AsyncGrpcOptimizer,
    AsyncPyomoClient,
    GrpcOptimizer,
    PyomoClient,
    SolveStatus,
    solve,
    solve_async,
)
from pyomo_client import build_model


async def check_pyomo(target: str) -> None:
    """Check sync and async solving, objective offsets, loading, reuse, and infeasibility."""
    # Exercise the public one-call APIs against the Rust server.
    model = build_model()
    model.profit.set_value(model.profit.expr + 7)
    result = await asyncio.to_thread(solve, model, endpoint=target, diagnostics=True, request_id="pyomo-sync")
    assert result.is_optimal and result.solution_loaded, result
    assert result.request_id == "pyomo-sync"
    assert math.isclose(pyo.value(model.x), 60, abs_tol=1e-6)
    assert math.isclose(pyo.value(model.y), 40, abs_tol=1e-6)
    assert result.objective is not None
    assert math.isclose(result.objective, 3607, abs_tol=1e-6)
    assert math.isclose(pyo.value(model.profit), result.objective, abs_tol=1e-6)
    assert result.raw.solution.HasField("dual")
    assert len(result.raw.solution.constraint_activities) == 2

    async_model = build_model()
    async_model.x.set_value(1)
    async_result = await solve_async(async_model, endpoint=target, load_solution=False)
    assert async_result.is_optimal and async_result.has_solution and not async_result.solution_loaded
    assert pyo.value(async_model.x) == 1
    assert async_result.objective is not None
    assert math.isclose(async_result.objective, 3600, abs_tol=1e-6)

    async with AsyncPyomoClient(target) as client:
        for _ in range(2):
            result = await client.solve(async_model)
            assert result.is_optimal and result.solution_loaded
        async_model.impossible = pyo.Constraint(expr=async_model.x <= -1)
        result = await client.solve(async_model)
        assert result.status == SolveStatus.INFEASIBLE and not result.has_solution
        assert math.isclose(pyo.value(async_model.x), 60, abs_tol=1e-6)

    def check_sync_reuse():
        """Verify repeated solves through one synchronous client against the live server."""
        with PyomoClient(target) as client:
            for _ in range(2):
                assert client.solve(build_model()).is_optimal

    await asyncio.to_thread(check_sync_reuse)


def session_model():
    """Build the production LP with machine hours as a mutable parameter."""
    model = build_model()
    model.hours = pyo.Param(initialize=100, mutable=True)
    model.machine.set_value(model.x + model.y <= model.hours)
    return model


async def check_optimizer(target: str) -> None:
    """Check that sync and async optimizers apply changed values to the live server's model."""

    def run_sync():
        model = session_model()
        with GrpcOptimizer(model, target, timeout=10) as optimizer:
            assert math.isclose(optimizer.solve().objective or 0, 3600, abs_tol=1e-6)
            model.hours = 50
            result = optimizer.solve()
            assert result.is_optimal and math.isclose(result.objective or 0, 2000, abs_tol=1e-6), result
            assert math.isclose(pyo.value(model.x), 50, abs_tol=1e-6)

    await asyncio.to_thread(run_sync)
    model = session_model()
    async with AsyncGrpcOptimizer(model, target, timeout=10) as optimizer:
        model.hours = 50
        assert math.isclose((await optimizer.solve()).objective or 0, 2000, abs_tol=1e-6)
        model.hours = 100
        assert math.isclose((await optimizer.solve()).objective or 0, 3600, abs_tol=1e-6)


async def main() -> None:
    """Verify Pyomo APIs, fixture results, streaming errors, and deadlines against a live server."""
    target = os.environ.get("OPT_GRPC_TARGET", "127.0.0.1:50051")
    async with grpc.aio.insecure_channel(target) as channel:
        await asyncio.wait_for(channel.channel_ready(), 10)
        await check_pyomo(target)
        await check_optimizer(target)
        stub = rpc.OptimizerStub(channel)
        for path in sorted((ROOT / "examples/models").glob("*.json")):
            sample, jobs = load_jobs(path, 2, diagnostics=True)
            for mode in MODES:
                received = [o async for o in outcomes(stub, mode, jobs, 10)]
                assert len(received) == 2, (sample["name"], mode)
                for outcome in received:
                    assert outcome.HasField("result"), outcome
                    result = outcome.result
                    assert result.status == sample["expected_status"], result
                    if sample["expected_objective"] is not None:
                        assert result.HasField("solution"), result
                        assert math.isclose(
                            result.solution.objective,
                            sample["expected_objective"],
                            abs_tol=1e-6,
                        )
                        assert result.solution.HasField("dual"), result
                    if result.status == pb.SOLVE_STATUS_INFEASIBLE:
                        assert not result.HasField("solution"), result
        for mode in MODES[1:]:
            _, jobs = load_jobs(ROOT / "examples/models/production.json", 2, invalid_first=True)
            received = [o async for o in outcomes(stub, mode, jobs, 10)]
            assert any(o.HasField("error") and o.error.code == 3 for o in received)
            assert any(o.HasField("result") for o in received)

        # Keep the request side open until the first result arrives.
        _, jobs = load_jobs(ROOT / "examples/models/production.json", 1)
        call = stub.SolveStream(timeout=10)
        await call.write(jobs[0])
        first = await asyncio.wait_for(call.read(), 5)
        assert first.HasField("result")
        await call.done_writing()
        assert await call.read() is grpc.aio.EOF

        # An open request side with no jobs makes deadline expiry deterministic.
        call = stub.SolveStream(timeout=0.03)
        try:
            await call.read()
        except grpc.aio.AioRpcError as error:
            assert error.code() in (
                grpc.StatusCode.DEADLINE_EXCEEDED,
                grpc.StatusCode.CANCELLED,
            )
        else:
            raise AssertionError("expired call unexpectedly succeeded")
    print("Python interoperability: Pyomo, sessions, all fixtures, unary and bidi passed")


if __name__ == "__main__":
    asyncio.run(main())
