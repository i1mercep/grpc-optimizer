"""Interoperability checks against a running grpc-optimizer-server."""

import asyncio
import math
import os
from collections.abc import AsyncIterator

import pyomo.environ as pyo

from client import MODES, ROOT, grpc, load_jobs, outcomes, pb, rpc
from grpc_optimizer import AsyncPyomoClient, PyomoClient, SolveStatus, solve, solve_async
from pyomo_client import build_model


async def check_pyomo(target: str) -> None:
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
        with PyomoClient(target) as client:
            for _ in range(2):
                assert client.solve(build_model()).is_optimal

    await asyncio.to_thread(check_sync_reuse)


async def main() -> None:
    target = os.environ.get("OPT_GRPC_TARGET", "127.0.0.1:50051")
    async with grpc.aio.insecure_channel(target) as channel:
        await asyncio.wait_for(channel.channel_ready(), 10)
        await check_pyomo(target)
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

        async def blocked_requests() -> AsyncIterator[pb.SolveRequest]:
            # A tiny solve may beat the client's rounded timeout. Keep the
            # request open instead, so deadline enforcement is deterministic.
            await asyncio.Event().wait()
            yield jobs[0]

        try:
            await stub.Collect(blocked_requests(), timeout=0.03)
        except grpc.aio.AioRpcError as error:
            assert error.code() in (
                grpc.StatusCode.DEADLINE_EXCEEDED,
                grpc.StatusCode.CANCELLED,
            )
        else:
            raise AssertionError("expired call unexpectedly succeeded")
    print("Python interoperability: Pyomo, all fixtures and four RPC patterns passed")


if __name__ == "__main__":
    asyncio.run(main())
