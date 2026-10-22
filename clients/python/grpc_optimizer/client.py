"""Synchronous and asynchronous Pyomo solving with explicit channel ownership."""

import asyncio
import math
import os
import queue
import threading
from collections.abc import Callable, Iterator
from typing import Any, Self
from uuid import uuid4

import grpc

from ._generated.optimization.v1 import optimizer_pb2 as pb
from ._generated.optimization.v1 import optimizer_pb2_grpc as rpc
from .adapter import PreparedModel, prepare_model, session_input
from .result import ProtocolError, SolveResult, finish_outcome, finish_solve

_CHANNEL_OPTIONS = (
    ("grpc.max_receive_message_length", 16 * 1024 * 1024),
    ("grpc.max_send_message_length", 16 * 1024 * 1024),
)


def _positive(value: float | None, name: str) -> None:
    if value is not None and not (math.isfinite(value) and value > 0):
        raise ValueError(f"{name} must be finite and positive")


def _prepare(model: Any, request_id: str | None, time_limit: float | None, diagnostics: bool) -> PreparedModel:
    """Validate the solver budget and capture the model with a request ID and solver options."""
    _positive(time_limit, "time_limit")
    prepared = prepare_model(model, uuid4().hex if request_id is None else request_id)
    prepared.request.diagnostics = diagnostics
    if time_limit is not None:
        prepared.request.time_limit_seconds = time_limit
    return prepared


def _target(endpoint: str | None) -> str:
    """Resolve an explicit endpoint, OPT_GRPC_TARGET, or loopback; reject blank targets."""
    target = os.environ.get("OPT_GRPC_TARGET", "127.0.0.1:50051") if endpoint is None else endpoint
    if not target.strip():
        raise ValueError("endpoint must not be empty")
    return target


def _connect[C](endpoint: str | None, channel: C | None, factory: Callable[..., C]) -> tuple[C, bool]:
    """Return the channel and whether it is owned; borrow the caller's or create one, never both."""
    if endpoint is not None and channel is not None:
        raise ValueError("provide endpoint or channel, not both")
    if channel is not None:
        return channel, False
    return factory(_target(endpoint), options=_CHANNEL_OPTIONS), True


def _drain(requests: queue.Queue[pb.SessionRequest | None]) -> Iterator[pb.SessionRequest]:
    while (message := requests.get()) is not None:
        yield message


class GrpcOptimizer:
    """Solve one Pyomo model repeatedly, sending its structure only once.

    Between solves, change mutable Params, bounds, or objective coefficients; each
    solve sends only values that differ from the model at construction. Structural
    changes raise ValueError; create a new optimizer for them. Solves run one at a
    time. The open call holds a server slot: use a context manager or call close().
    """

    def __init__(
        self,
        model: Any,
        endpoint: str | None = None,
        *,
        channel: grpc.Channel | None = None,
        timeout: float | None = None,
    ) -> None:
        """Send the model over a new or borrowed channel; timeout=None keeps the call open until close()."""
        _positive(timeout, "timeout")
        self._model = model
        self._base = prepare_model(model, "base").request.model
        self._channel, self._owned = _connect(endpoint, channel, grpc.insecure_channel)
        self._requests: queue.Queue[pb.SessionRequest | None] = queue.Queue()
        self._requests.put(pb.SessionRequest(model=self._base))
        self._responses = rpc.OptimizerStub(self._channel).SolveSession(_drain(self._requests), timeout=timeout)
        self._lock = threading.Lock()
        self._closed = False

    def solve(
        self,
        *,
        time_limit: float | None = None,
        diagnostics: bool = False,
        request_id: str | None = None,
        load_solution: bool = True,
    ) -> SolveResult:
        """Solve the model's current values and load any feasible solution into it.

        A rejected job raises JobError and keeps the optimizer open; RPC failures
        raise grpc.RpcError and close it.
        """
        prepared = _prepare(self._model, request_id, time_limit, diagnostics)
        message = pb.SessionRequest(input=session_input(self._base, prepared.request))
        with self._lock:
            if self._closed:
                raise RuntimeError("optimizer is closed")
            try:
                self._requests.put(message)
                outcome = next(self._responses, None)
                if outcome is None:
                    raise ProtocolError("server ended the call before returning a result")
            except BaseException:
                self.close()
                raise
        return finish_outcome(prepared, outcome, load_solution=load_solution)

    def close(self) -> None:
        """End the call and close an owned channel once, interrupting a pending solve."""
        if not self._closed:
            self._closed = True
            self._requests.put(None)
            self._responses.cancel()
            if self._owned:
                self._channel.close()

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *_exc: object) -> None:
        self.close()


class AsyncGrpcOptimizer:
    """Async counterpart of GrpcOptimizer; create and use within one event loop.

    Cancelling a pending solve closes the call.
    """

    def __init__(
        self,
        model: Any,
        endpoint: str | None = None,
        *,
        channel: grpc.aio.Channel | None = None,
        timeout: float | None = None,
    ) -> None:
        """Open the call over a new or borrowed channel; the model is sent with the first solve."""
        _positive(timeout, "timeout")
        self._model = model
        self._base = prepare_model(model, "base").request.model
        self._channel, self._owned = _connect(endpoint, channel, grpc.aio.insecure_channel)
        self._call = rpc.OptimizerStub(self._channel).SolveSession(timeout=timeout)
        self._lock = asyncio.Lock()
        self._started = False
        self._closed = False

    async def solve(
        self,
        *,
        time_limit: float | None = None,
        diagnostics: bool = False,
        request_id: str | None = None,
        load_solution: bool = True,
    ) -> SolveResult:
        """Await a solve with the same options and errors as GrpcOptimizer.solve."""
        prepared = _prepare(self._model, request_id, time_limit, diagnostics)
        message = pb.SessionRequest(input=session_input(self._base, prepared.request))
        async with self._lock:
            if self._closed:
                raise RuntimeError("optimizer is closed")
            try:
                if not self._started:
                    await self._call.write(pb.SessionRequest(model=self._base))
                    self._started = True
                await self._call.write(message)
                outcome = await self._call.read()
                if outcome is grpc.aio.EOF:
                    raise ProtocolError("server ended the call before returning a result")
            except BaseException:
                self._closed = True
                self._call.cancel()
                raise
        return finish_outcome(prepared, outcome, load_solution=load_solution)

    async def close(self) -> None:
        """End the call and close an owned channel once, interrupting a pending solve."""
        self._closed = True
        self._call.cancel()
        if self._owned:
            self._owned = False
            await self._channel.close()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.close()


class PyomoClient:
    """Solve many models over one channel. Use a context manager or call close().

    To re-solve one model with changing data, use GrpcOptimizer. An injected
    channel stays owned by the caller (including TLS channels). Models must not
    be modified while their solve is in progress.
    """

    def __init__(self, endpoint: str | None = None, *, channel: grpc.Channel | None = None) -> None:
        """Create an owned channel or borrow the caller's channel, but never accept both inputs."""
        self._channel, self._owned = _connect(endpoint, channel, grpc.insecure_channel)
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
        _positive(timeout, "timeout")
        prepared = _prepare(model, request_id, time_limit, diagnostics)
        response = self._stub.Solve(prepared.request, timeout=timeout)
        return finish_solve(prepared, response, load_solution=load_solution)

    def close(self) -> None:
        """Disable further solves and close an owned channel once; leave borrowed channels open."""
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
        """Create an owned async channel or borrow one bound to the current event loop."""
        self._channel, self._owned = _connect(endpoint, channel, grpc.aio.insecure_channel)
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
        _positive(timeout, "timeout")
        prepared = _prepare(model, request_id, time_limit, diagnostics)
        response = await self._stub.Solve(prepared.request, timeout=timeout)
        return finish_solve(prepared, response, load_solution=load_solution)

    async def close(self) -> None:
        """Disable further solves and await closure of an owned channel once."""
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
    """Solve a continuous LP with a temporary channel, optionally loading its solution."""
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
    """Await a solve with a temporary channel, optionally loading values into the model."""
    async with AsyncPyomoClient(endpoint) as client:
        return await client.solve(
            model,
            timeout=timeout,
            time_limit=time_limit,
            diagnostics=diagnostics,
            request_id=request_id,
            load_solution=load_solution,
        )
