"""Synchronous and asynchronous Pyomo solving with explicit channel ownership."""

import math
import os
from typing import Any, Self
from uuid import uuid4

import grpc

from ._generated.optimization.v1 import optimizer_pb2_grpc as rpc
from .adapter import PreparedModel, prepare_model
from .result import SolveResult, finish_solve

_CHANNEL_OPTIONS = (
    ("grpc.max_receive_message_length", 16 * 1024 * 1024),
    ("grpc.max_send_message_length", 16 * 1024 * 1024),
)


def _positive(value: float, name: str) -> None:
    if not math.isfinite(value) or value <= 0:
        raise ValueError(f"{name} must be finite and positive")


def _prepare(
    model: Any, request_id: str | None, timeout: float, time_limit: float | None, diagnostics: bool
) -> PreparedModel:
    _positive(timeout, "timeout")
    if time_limit is not None:
        _positive(time_limit, "time_limit")
    prepared = prepare_model(model, uuid4().hex if request_id is None else request_id)
    prepared.request.diagnostics = diagnostics
    if time_limit is not None:
        prepared.request.time_limit_seconds = time_limit
    return prepared


def _target(endpoint: str | None) -> str:
    target = os.environ.get("OPT_GRPC_TARGET", "127.0.0.1:50051") if endpoint is None else endpoint
    if not target.strip():
        raise ValueError("endpoint must not be empty")
    return target


class PyomoClient:
    """Reuse one channel across solves. Use a context manager or call close().

    An injected channel stays owned by the caller (including TLS channels).
    Models must not be modified while their solve is in progress.
    """

    def __init__(self, endpoint: str | None = None, *, channel: grpc.Channel | None = None) -> None:
        if endpoint is not None and channel is not None:
            raise ValueError("provide endpoint or channel, not both")
        self._owned = channel is None
        self._channel = (
            grpc.insecure_channel(_target(endpoint), options=_CHANNEL_OPTIONS) if channel is None else channel
        )
        self._stub = rpc.OptimizerStub(self._channel)
        self._closed = False

    def solve(
        self,
        model: Any,
        *,
        timeout: float = 30,
        time_limit: float | None = None,
        diagnostics: bool = False,
        request_id: str | None = None,
        load_solution: bool = True,
    ) -> SolveResult:
        """Send a continuous LP and load any feasible solution into its variables.

        timeout covers the RPC, including connection and queueing. time_limit
        optionally limits solver time. RPC failures propagate as grpc.RpcError;
        infeasibility and solver limits are returned as SolveStatus values.
        """
        if self._closed:
            raise RuntimeError("client is closed")
        prepared = _prepare(model, request_id, timeout, time_limit, diagnostics)
        response = self._stub.Solve(prepared.request, timeout=timeout)
        return finish_solve(prepared, response, load_solution=load_solution)

    def close(self) -> None:
        if not self._closed and self._owned:
            self._channel.close()
        self._closed = True

    def __enter__(self) -> Self:
        if self._closed:
            raise RuntimeError("client is closed")
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()


class AsyncPyomoClient:
    """Async counterpart of PyomoClient; create and use within one event loop."""

    def __init__(self, endpoint: str | None = None, *, channel: grpc.aio.Channel | None = None) -> None:
        if endpoint is not None and channel is not None:
            raise ValueError("provide endpoint or channel, not both")
        self._owned = channel is None
        self._channel = (
            grpc.aio.insecure_channel(_target(endpoint), options=_CHANNEL_OPTIONS) if channel is None else channel
        )
        self._stub = rpc.OptimizerStub(self._channel)
        self._closed = False

    async def solve(
        self,
        model: Any,
        *,
        timeout: float = 30,  # noqa: ASYNC109 -- passed to gRPC as a server-visible deadline.
        time_limit: float | None = None,
        diagnostics: bool = False,
        request_id: str | None = None,
        load_solution: bool = True,
    ) -> SolveResult:
        """Await a solve with the same options and loading rules as PyomoClient.solve."""
        if self._closed:
            raise RuntimeError("client is closed")
        prepared = _prepare(model, request_id, timeout, time_limit, diagnostics)
        response = await self._stub.Solve(prepared.request, timeout=timeout)
        return finish_solve(prepared, response, load_solution=load_solution)

    async def close(self) -> None:
        if not self._closed and self._owned:
            await self._channel.close()
        self._closed = True

    async def __aenter__(self) -> Self:
        if self._closed:
            raise RuntimeError("client is closed")
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.close()


def solve(
    model: Any,
    *,
    endpoint: str | None = None,
    timeout: float = 30,
    time_limit: float | None = None,
    diagnostics: bool = False,
    request_id: str | None = None,
    load_solution: bool = True,
) -> SolveResult:
    """Solve a Pyomo continuous LP and update its variables with one function call."""
    with PyomoClient(endpoint) as client:
        return client.solve(
            model,
            timeout=timeout,
            time_limit=time_limit,
            diagnostics=diagnostics,
            request_id=request_id,
            load_solution=load_solution,
        )


async def solve_async(
    model: Any,
    *,
    endpoint: str | None = None,
    timeout: float = 30,  # noqa: ASYNC109 -- same deadline option as the synchronous API.
    time_limit: float | None = None,
    diagnostics: bool = False,
    request_id: str | None = None,
    load_solution: bool = True,
) -> SolveResult:
    """Async one-call solve; suitable for notebooks and existing event loops."""
    async with AsyncPyomoClient(endpoint) as client:
        return await client.solve(
            model,
            timeout=timeout,
            time_limit=time_limit,
            diagnostics=diagnostics,
            request_id=request_id,
            load_solution=load_solution,
        )
