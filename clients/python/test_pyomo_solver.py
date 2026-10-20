"""SDK behavior checks with transport doubles; no running server required."""

import asyncio
import math
import unittest
from unittest.mock import AsyncMock, Mock, patch

import grpc
import pyomo.environ as pyo

from grpc_optimizer import AsyncPyomoClient, ProtocolError, PyomoClient, SolveStatus, solve, solve_async
from grpc_optimizer._generated.optimization.v1 import optimizer_pb2 as pb


def model():
    m = pyo.ConcreteModel()
    m.x = pyo.Var(initialize=1)
    m.y = pyo.Var(initialize=2)
    m.fixed = pyo.Var(initialize=4)
    m.fixed.fix()
    m.objective = pyo.Objective(expr=3 * m.x + 2 * m.y + m.fixed + 7)
    return m


def response(request, **_kwargs):
    return pb.SolveResult(
        request_id=request.request_id,
        status=pb.SOLVE_STATUS_OPTIMAL,
        solution=pb.Solution(objective=22, variable_values=[4, 5]),
        solver_status="Optimal",
    )


class SolverTests(unittest.TestCase):
    def setUp(self):
        self.channel = Mock(spec=grpc.Channel)
        self.stub = Mock()
        self.stub.Solve.side_effect = response
        self.factory = patch("grpc_optimizer.client.rpc.OptimizerStub", return_value=self.stub)
        self.factory.start()
        self.addCleanup(self.factory.stop)

    def test_solves_and_loads_objective_including_fixed_variable_and_offset(self):
        m = model()
        with PyomoClient(channel=self.channel) as client:
            result = client.solve(m, request_id="example", diagnostics=True, time_limit=5, timeout=8)
        self.assertEqual((pyo.value(m.x), pyo.value(m.y), pyo.value(m.fixed)), (4, 5, 4))
        self.assertEqual(result.objective, pyo.value(m.objective))
        self.assertEqual(result.objective, 33)
        self.assertTrue(result.is_optimal and result.has_solution and result.solution_loaded)
        self.assertEqual(result.solver_status, "Optimal")
        request = self.stub.Solve.call_args.args[0]
        self.assertTrue(request.diagnostics)
        self.assertEqual(request.time_limit_seconds, 5)
        self.assertEqual(self.stub.Solve.call_args.kwargs, {"timeout": 8})
        self.channel.close.assert_not_called()

    def test_loading_can_be_disabled_and_objective_is_still_correct(self):
        m = model()
        with PyomoClient(channel=self.channel) as client:
            result = client.solve(m, load_solution=False)
        self.assertEqual((pyo.value(m.x), pyo.value(m.y)), (1, 2))
        self.assertEqual(result.objective, 33)
        self.assertTrue(result.has_solution)
        self.assertFalse(result.solution_loaded)

    def test_all_nonoptimal_statuses_without_solution_preserve_values(self):
        for status in SolveStatus:
            if status == SolveStatus.OPTIMAL:
                continue
            with self.subTest(status=status):
                self.stub.Solve.side_effect = lambda request, status=status, **_: pb.SolveResult(
                    request_id=request.request_id, status=f"SOLVE_STATUS_{status.name}"
                )
                m = model()
                with PyomoClient(channel=self.channel) as client:
                    result = client.solve(m)
                self.assertEqual((pyo.value(m.x), pyo.value(m.y)), (1, 2))
                self.assertFalse(result.has_solution or result.solution_loaded or result.is_optimal)
                self.assertIsNone(result.objective)

    def test_feasible_time_limit_solution_is_loaded_and_not_reported_optimal(self):
        def limited(request, **kwargs):
            result = response(request, **kwargs)
            result.status = pb.SOLVE_STATUS_TIME_LIMIT
            return result

        self.stub.Solve.side_effect = limited
        with PyomoClient(channel=self.channel) as client:
            result = client.solve(model())
        self.assertTrue(result.solution_loaded)
        self.assertFalse(result.is_optimal)
        self.assertEqual(result.status, SolveStatus.TIME_LIMIT)

    def test_malformed_response_never_partially_updates_model(self):
        def wrong_id(result):
            result.request_id = "wrong"

        def too_short(result):
            del result.solution.variable_values[-1]

        def too_long(result):
            result.solution.variable_values.append(6)

        def nonfinite(result):
            result.solution.variable_values[-1] = math.nan

        def invalid_objective(result):
            result.solution.objective = math.inf

        def missing_solution(result):
            result.ClearField("solution")

        def invalid_status(result):
            result.status = 999

        for corrupt in (wrong_id, too_short, too_long, nonfinite, invalid_objective, missing_solution, invalid_status):
            with self.subTest(case=corrupt.__name__):

                def malformed(request, corrupt=corrupt, **kwargs):
                    result = response(request, **kwargs)
                    corrupt(result)
                    return result

                self.stub.Solve.side_effect = malformed
                m = model()
                with PyomoClient(channel=self.channel) as client, self.assertRaises(ProtocolError):
                    client.solve(m)
                self.assertEqual((pyo.value(m.x), pyo.value(m.y)), (1, 2))

    def test_invalid_model_and_options_fail_before_rpc(self):
        with PyomoClient(channel=self.channel) as client:
            for value in (0, -1, math.inf, math.nan):
                with self.subTest(timeout=value), self.assertRaises(ValueError):
                    client.solve(model(), timeout=value)
                with self.subTest(time_limit=value), self.assertRaises(ValueError):
                    client.solve(model(), time_limit=value)
            with self.assertRaises(ValueError):
                client.solve(model(), request_id="")
            m = model()
            m.objective.set_value(m.x**2)
            with self.assertRaisesRegex(ValueError, "linear"):
                client.solve(m)
        self.stub.Solve.assert_not_called()

    def test_connection_is_reused_and_requests_have_distinct_ids(self):
        with patch("grpc_optimizer.client.grpc.insecure_channel", return_value=self.channel) as factory:
            with PyomoClient("server:50051") as client:
                first = client.solve(model())
                second = client.solve(model())
            self.assertNotEqual(first.request_id, second.request_id)
            factory.assert_called_once()
            self.channel.close.assert_called_once()
            client.close()
            self.channel.close.assert_called_once()
            with self.assertRaisesRegex(RuntimeError, "closed"):
                client.solve(model())

    def test_one_call_uses_environment_target_and_closes_owned_channel(self):
        with (
            patch.dict("os.environ", {"OPT_GRPC_TARGET": "remote:50051"}),
            patch("grpc_optimizer.client.grpc.insecure_channel", return_value=self.channel) as factory,
        ):
            self.assertTrue(solve(model()).is_optimal)
        self.assertEqual(factory.call_args.args[0], "remote:50051")
        self.channel.close.assert_called_once()

    def test_explicit_endpoint_takes_precedence_and_default_is_loopback(self):
        with (
            patch.dict("os.environ", {}, clear=True),
            patch("grpc_optimizer.client.grpc.insecure_channel", return_value=self.channel) as factory,
        ):
            solve(model())
            self.assertEqual(factory.call_args.args[0], "127.0.0.1:50051")
            with patch.dict("os.environ", {"OPT_GRPC_TARGET": "remote:50051"}):
                solve(model(), endpoint="explicit:50051")
            self.assertEqual(factory.call_args.args[0], "explicit:50051")

    def test_endpoint_and_channel_are_mutually_exclusive(self):
        with self.assertRaises(ValueError):
            PyomoClient("server:50051", channel=self.channel)
        with self.assertRaises(ValueError):
            PyomoClient("")

    def test_rpc_errors_propagate_and_close_owned_channel_without_loading(self):
        error = grpc.RpcError("unavailable")
        self.stub.Solve.side_effect = error
        m = model()
        with (
            patch("grpc_optimizer.client.grpc.insecure_channel", return_value=self.channel),
            self.assertRaises(grpc.RpcError) as caught,
        ):
            solve(m)
        self.assertIs(caught.exception, error)
        self.assertEqual(pyo.value(m.x), 1)
        self.channel.close.assert_called_once()


class AsyncSolverTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.channel = Mock(spec=grpc.aio.Channel)
        self.channel.close = AsyncMock()
        self.stub = Mock()
        self.stub.Solve = AsyncMock(side_effect=response)
        self.factory = patch("grpc_optimizer.client.rpc.OptimizerStub", return_value=self.stub)
        self.factory.start()
        self.addCleanup(self.factory.stop)

    async def test_one_call_async_loads_and_closes_owned_channel(self):
        m = model()
        with patch("grpc_optimizer.client.grpc.aio.insecure_channel", return_value=self.channel):
            result = await solve_async(m, request_id="async", timeout=2, time_limit=1, diagnostics=True)
        self.assertEqual(result.objective, 33)
        self.assertEqual(pyo.value(m.x), 4)
        self.assertEqual(result.request_id, "async")
        request = self.stub.Solve.call_args.args[0]
        self.assertEqual(request.time_limit_seconds, 1)
        self.assertTrue(request.diagnostics)
        self.assertEqual(self.stub.Solve.call_args.kwargs, {"timeout": 2})
        self.channel.close.assert_awaited_once()

    async def test_async_client_reuses_borrowed_channel_and_respects_loading_option(self):
        m = model()
        async with AsyncPyomoClient(channel=self.channel) as client:
            first = await client.solve(m, load_solution=False)
            second = await client.solve(m)
        self.assertFalse(first.solution_loaded)
        self.assertTrue(second.solution_loaded)
        self.assertNotEqual(first.request_id, second.request_id)
        self.channel.close.assert_not_awaited()
        with self.assertRaisesRegex(RuntimeError, "closed"):
            await client.solve(m)

    async def test_async_cancellation_preserves_model_and_closes_channel(self):
        self.stub.Solve.side_effect = asyncio.CancelledError
        m = model()
        with (
            patch("grpc_optimizer.client.grpc.aio.insecure_channel", return_value=self.channel),
            self.assertRaises(asyncio.CancelledError),
        ):
            await solve_async(m)
        self.assertEqual(pyo.value(m.x), 1)
        self.channel.close.assert_awaited_once()


if __name__ == "__main__":
    unittest.main()
