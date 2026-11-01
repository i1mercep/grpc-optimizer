"""Async examples of unary and bidirectional gRPC calls. Generate stubs before running."""

import asyncio
import json
import sys
from collections.abc import AsyncIterator, Sequence
from pathlib import Path
from typing import Any

import click
import grpc
from google.protobuf.json_format import MessageToDict, ParseDict

from grpc_optimizer._generated.optimization.v1 import optimizer_pb2 as pb
from grpc_optimizer._generated.optimization.v1 import optimizer_pb2_grpc as rpc
from grpc_optimizer.client import _metadata

ROOT = Path(__file__).resolve().parents[2]
MODES = ("unary", "bidi")


def load_jobs(
    fixture: str | Path,
    count: int,
    diagnostics: bool = False,
    invalid_first: bool = False,
) -> tuple[dict[str, Any], list[pb.SolveRequest]]:
    """Clone a JSON fixture into uniquely named jobs, optionally invalidating the first."""
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
    access_token: str | None = None,
) -> AsyncIterator[pb.JobOutcome]:
    """Yield uniform job outcomes for either RPC mode, wrapping unary results.

    Bidirectional mode sends concurrently with receiving and cancels both the
    call and its producer when iteration exits.
    """
    if mode == "unary":
        for job in jobs:
            result = await stub.Solve(job, timeout=rpc_timeout, metadata=_metadata(access_token))
            yield pb.JobOutcome(request_id=result.request_id, result=result)
    else:
        call = stub.SolveStream(timeout=rpc_timeout, metadata=_metadata(access_token))

        async def produce() -> None:
            """Write every job, then half-close the stream so responses can finish."""
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


async def run(
    endpoint: str,
    mode: str,
    fixture: Path,
    jobs: int,
    deadline: float,
    diagnostics: bool,
    invalid_first: bool,
    access_token: str | None,
    tls: bool,
) -> None:
    """Wait for a connection, submit fixture jobs, and print JSON outcomes and job errors."""
    _, requests = load_jobs(fixture, jobs, diagnostics, invalid_first)
    options = [
        ("grpc.max_receive_message_length", 16 * 1024 * 1024),
        ("grpc.max_send_message_length", 16 * 1024 * 1024),
    ]
    # Native gRPC targets are host:port, whereas Rust Tonic endpoints use http://.
    _metadata(access_token)
    channel = (
        grpc.aio.secure_channel(endpoint, grpc.ssl_channel_credentials(), options=options)
        if tls
        else grpc.aio.insecure_channel(endpoint, options=options)
    )
    async with channel:
        await asyncio.wait_for(channel.channel_ready(), timeout=5)
        stub = rpc.OptimizerStub(channel)
        async for outcome in outcomes(stub, mode, requests, deadline, access_token):
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


@click.command(help=__doc__)
@click.option("--endpoint", default="127.0.0.1:50051", envvar="OPT_GRPC_TARGET")
@click.option("--access-token", envvar="OPT_ACCESS_TOKEN", help="IAM access token (or OPT_ACCESS_TOKEN)")
@click.option("--tls", is_flag=True, help="use TLS with trusted server certificates")
@click.option("--mode", type=click.Choice(MODES), default="unary")
@click.option("--fixture", type=click.Path(path_type=Path), default=ROOT / "examples/models/production.json")
@click.option("--jobs", type=click.IntRange(1, 64), default=3)
@click.option("--deadline", type=float, default=30)
@click.option("--diagnostics", is_flag=True)
@click.option("--invalid-first", is_flag=True)
def main(
    endpoint: str,
    access_token: str | None,
    tls: bool,
    mode: str,
    fixture: Path,
    jobs: int,
    deadline: float,
    diagnostics: bool,
    invalid_first: bool,
) -> None:
    """Validate CLI limits and run the selected RPC mode, reporting transport failures."""
    if not 0 < deadline <= 86400:
        raise click.BadParameter("must be in (0,86400] seconds", param_hint="--deadline")
    try:
        asyncio.run(run(endpoint, mode, fixture, jobs, deadline, diagnostics, invalid_first, access_token, tls))
    except grpc.aio.AioRpcError as error:
        print(f"RPC failed: {error.code().name}: {error.details()}", file=sys.stderr)
        raise SystemExit(1) from error


if __name__ == "__main__":
    main()
