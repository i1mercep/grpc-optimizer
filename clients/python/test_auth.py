"""Credential propagation and secure-channel ownership for each SDK entry point."""

import unittest
from unittest.mock import AsyncMock, Mock, patch

import grpc
from test_pyomo_solver import FakeAsyncSessionCall, FakeSessionCall, model, response, session_model

from grpc_optimizer import AsyncGrpcOptimizer, AsyncPyomoClient, GrpcOptimizer, PyomoClient, solve, solve_async

METADATA = (("authorization", "Bearer test-token"),)


class AuthenticationTests(unittest.TestCase):
    def test_sync_clients_attach_tokens_to_unary_and_session_calls(self):
        channel = Mock(spec=grpc.Channel)
        stub = Mock()
        stub.Solve.side_effect = response
        stub.SolveSession.side_effect = FakeSessionCall
        with patch("grpc_optimizer.client.rpc.OptimizerStub", return_value=stub):
            with PyomoClient(channel=channel, access_token="test-token") as client:
                client.solve(model())
                client.solve(model())
            self.assertTrue(all(call.kwargs["metadata"] == METADATA for call in stub.Solve.call_args_list))
            with GrpcOptimizer(session_model(), channel=channel, access_token="test-token") as optimizer:
                optimizer.solve()
                optimizer.solve()
            self.assertEqual(stub.SolveSession.call_count, 1)
            self.assertEqual(stub.SolveSession.call_args.kwargs["metadata"], METADATA)
        channel.close.assert_not_called()

    def test_secure_channel_and_one_call_token_forwarding(self):
        channel = Mock(spec=grpc.Channel)
        stub = Mock()
        stub.Solve.side_effect = response
        with (
            patch("grpc_optimizer.client.grpc.secure_channel", return_value=channel) as secure,
            patch("grpc_optimizer.client.grpc.insecure_channel") as insecure,
            patch("grpc_optimizer.client.rpc.OptimizerStub", return_value=stub),
        ):
            solve(model(), endpoint="optimizer.example:443", access_token="test-token", tls=True)
        secure.assert_called_once()
        self.assertEqual(secure.call_args.args[0], "optimizer.example:443")
        insecure.assert_not_called()
        self.assertEqual(stub.Solve.call_args.kwargs["metadata"], METADATA)
        channel.close.assert_called_once()

    def test_invalid_tokens_and_conflicting_tls_fail_before_connection(self):
        for token in ("", "Bearer token", "one\ntwo", "one,two", "é", "x" * (16 * 1024 + 1)):
            with self.subTest(token_length=len(token)), patch("grpc_optimizer.client.grpc.insecure_channel") as factory:
                with self.assertRaisesRegex(ValueError, "^invalid access token$"):
                    PyomoClient(access_token=token)
                factory.assert_not_called()
        channel = Mock(spec=grpc.Channel)
        with self.assertRaisesRegex(ValueError, "supplied channel"):
            PyomoClient(channel=channel, tls=True)
        channel.close.assert_not_called()


class AsyncAuthenticationTests(unittest.IsolatedAsyncioTestCase):
    async def test_async_clients_attach_tokens_and_preserve_borrowed_channels(self):
        channel = Mock(spec=grpc.aio.Channel)
        channel.close = AsyncMock()
        stub = Mock()
        stub.Solve = AsyncMock(side_effect=response)
        stub.SolveSession.side_effect = FakeAsyncSessionCall
        with patch("grpc_optimizer.client.rpc.OptimizerStub", return_value=stub):
            async with AsyncPyomoClient(channel=channel, access_token="test-token") as client:
                await client.solve(model())
            self.assertEqual(stub.Solve.call_args.kwargs["metadata"], METADATA)
            async with AsyncGrpcOptimizer(session_model(), channel=channel, access_token="test-token") as optimizer:
                await optimizer.solve()
                await optimizer.solve()
            self.assertEqual(stub.SolveSession.call_count, 1)
            self.assertEqual(stub.SolveSession.call_args.kwargs["metadata"], METADATA)
        channel.close.assert_not_awaited()

    async def test_async_secure_channel_and_one_call_forwarding(self):
        channel = Mock(spec=grpc.aio.Channel)
        channel.close = AsyncMock()
        stub = Mock()
        stub.Solve = AsyncMock(side_effect=response)
        with (
            patch("grpc_optimizer.client.grpc.aio.secure_channel", return_value=channel) as secure,
            patch("grpc_optimizer.client.grpc.aio.insecure_channel") as insecure,
            patch("grpc_optimizer.client.rpc.OptimizerStub", return_value=stub),
        ):
            await solve_async(model(), endpoint="optimizer.example:443", access_token="test-token", tls=True)
        secure.assert_called_once()
        insecure.assert_not_called()
        self.assertEqual(stub.Solve.call_args.kwargs["metadata"], METADATA)
        channel.close.assert_awaited_once()
