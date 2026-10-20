"""Interoperability checks against a running grpc-optimizer-server."""

import asyncio
import math
import os

from client import MODES, ROOT, grpc, load_jobs, outcomes, pb, rpc


async def main():
    target = os.environ.get("OPT_GRPC_TARGET", "127.0.0.1:50051")
    async with grpc.aio.insecure_channel(target) as channel:
        await asyncio.wait_for(channel.channel_ready(), 10)
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
                        assert math.isclose(result.solution.objective, sample["expected_objective"], abs_tol=1e-6)
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
        async def blocked_requests():
            # A tiny solve may beat the client's rounded timeout. Keep the
            # request open instead, so deadline enforcement is deterministic.
            await asyncio.Event().wait()
            yield jobs[0]
        try:
            await stub.Collect(blocked_requests(), timeout=0.03)
        except grpc.aio.AioRpcError as error:
            assert error.code() in (grpc.StatusCode.DEADLINE_EXCEEDED,
                                    grpc.StatusCode.CANCELLED)
        else:
            raise AssertionError("expired call unexpectedly succeeded")
    print("Python interoperability: all fixtures and four RPC patterns passed")


if __name__ == "__main__":
    asyncio.run(main())
