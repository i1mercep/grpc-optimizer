"""Async examples of all four gRPC patterns. Generate stubs before running."""

import argparse
import asyncio
import json
import os
import sys
from collections.abc import AsyncIterator, Sequence
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).parent / "generated"))

import grpc
from google.protobuf.json_format import MessageToDict, ParseDict

from optimization.v1 import optimizer_pb2 as pb
from optimization.v1 import optimizer_pb2_grpc as rpc

ROOT = Path(__file__).resolve().parents[2]
MODES = ("unary", "server-stream", "client-stream", "bidi")


def load_jobs(
    fixture: str | Path,
    count: int,
    diagnostics: bool = False,
    invalid_first: bool = False,
) -> tuple[dict[str, Any], list[pb.SolveRequest]]:
    sample = json.loads(Path(fixture).read_text())
    jobs = []
    for index in range(count):
        job = ParseDict(sample["request"], pb.SolveRequest())
        job.request_id = f"{sample['name']}-{index}"
        job.diagnostics = diagnostics
        if invalid_first and index == 0:
            job.model.sense = pb.OBJECTIVE_SENSE_UNSPECIFIED
        jobs.append(job)
    return sample, jobs


async def outcomes(
    stub: rpc.OptimizerStub,
    mode: str,
    jobs: Sequence[pb.SolveRequest],
    rpc_timeout: float,
) -> AsyncIterator[pb.JobOutcome]:
    if mode == "unary":
        for job in jobs:
            result = await stub.Solve(job, timeout=rpc_timeout)
            yield pb.JobOutcome(request_id=result.request_id, result=result)
    elif mode == "server-stream":
        async for outcome in stub.SolveBatch(pb.BatchRequest(jobs=jobs), timeout=rpc_timeout):
            yield outcome
    elif mode == "client-stream":

        async def requests() -> AsyncIterator[pb.SolveRequest]:
            for job in jobs:
                yield job
                await asyncio.sleep(0)

        response = await stub.Collect(requests(), timeout=rpc_timeout)
        for outcome in response.outcomes:
            yield outcome
    else:
        call = stub.SolveStream(timeout=rpc_timeout)

        async def produce() -> None:
            for job in jobs:
                await call.write(job)
            await call.done_writing()

        producer = asyncio.create_task(produce())
        try:
            async for outcome in call:
                yield outcome
            await producer
        finally:
            call.cancel()
            producer.cancel()
            await asyncio.gather(producer, return_exceptions=True)


async def run(args: argparse.Namespace) -> None:
    _, jobs = load_jobs(args.fixture, args.jobs, args.diagnostics, args.invalid_first)
    options = [
        ("grpc.max_receive_message_length", 16 * 1024 * 1024),
        ("grpc.max_send_message_length", 16 * 1024 * 1024),
    ]
    # Native gRPC targets are host:port, whereas Rust Tonic endpoints use http://.
    async with grpc.aio.insecure_channel(args.endpoint, options=options) as channel:
        await asyncio.wait_for(channel.channel_ready(), timeout=5)
        stub = rpc.OptimizerStub(channel)
        async for outcome in outcomes(stub, args.mode, jobs, args.deadline):
            print(
                json.dumps(
                    MessageToDict(
                        outcome,
                        preserving_proto_field_name=True,
                        use_integers_for_enums=True,
                    )
                )
            )
            if outcome.HasField("error"):
                print(
                    f"job {outcome.request_id}: code {outcome.error.code}: {outcome.error.message}",
                    file=sys.stderr,
                )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", default=os.environ.get("OPT_GRPC_TARGET", "127.0.0.1:50051"))
    parser.add_argument("--mode", choices=MODES, default="unary")
    parser.add_argument("--fixture", type=Path, default=ROOT / "examples/models/production.json")
    parser.add_argument("--jobs", type=int, default=3)
    parser.add_argument("--deadline", type=float, default=30)
    parser.add_argument("--diagnostics", action="store_true")
    parser.add_argument("--invalid-first", action="store_true")
    args = parser.parse_args()
    if not 1 <= args.jobs <= 64 or not 0 < args.deadline <= 86400:
        parser.error("jobs must be 1..64 and deadline must be in (0,86400] seconds")
    try:
        asyncio.run(run(args))
    except grpc.aio.AioRpcError as error:
        print(f"RPC failed: {error.code().name}: {error.details()}", file=sys.stderr)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
