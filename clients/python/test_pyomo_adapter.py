"""Conversion checks that do not require a running solver or gRPC server."""

import unittest

import pyomo.environ as pyo
from pyomo.gdp import Disjunct

from grpc_optimizer.adapter import prepare_model
from pyomo_adapter import pb, to_solve_request
from pyomo_client import build_model


class PyomoAdapterTests(unittest.TestCase):
    def test_client_examples_support_linear_and_quadratic_models(self):
        linear = prepare_model(build_model(), "linear-example")
        self.assertEqual([variable.objective for variable in linear.request.model.variables], [40, 30])
        self.assertFalse(linear.request.model.quadratic_coefficients)
        quadratic = prepare_model(build_model(quadratic=True), "quadratic-example")
        self.assertEqual(quadratic.objective_constant, 5)
        self.assertEqual(list(quadratic.request.model.quadratic_coefficients), [1, 1])
        self.assertEqual(quadratic.request.model.constraints[0].upper, 2)

    def test_linear_program_and_problem_names_are_compatible(self):
        self.assertIs(pb.Problem, pb.LinearProgram)
        self.assertEqual(pb.Problem.DESCRIPTOR.full_name, "optimization.v1.LinearProgram")
        legacy = pb.LinearProgram(
            sense=pb.OBJECTIVE_SENSE_MINIMIZE,
            variables=[pb.Variable(objective=3, lower=0)],
        )
        request = pb.SolveRequest(request_id="legacy", model=legacy)
        restored = pb.SolveRequest.FromString(request.SerializeToString())
        self.assertEqual(restored.model, legacy)
        self.assertIsInstance(restored.model, pb.Problem)

    def model(self):
        model = pyo.ConcreteModel()
        model.x = pyo.Var()
        model.objective = pyo.Objective(expr=3 * model.x)
        return model

    def test_objective_sense_and_unbounded_variable(self):
        for sense, expected in (
            (pyo.minimize, pb.OBJECTIVE_SENSE_MINIMIZE),
            (pyo.maximize, pb.OBJECTIVE_SENSE_MAXIMIZE),
        ):
            with self.subTest(sense=sense):
                model = self.model()
                model.objective.sense = sense
                request, variables = to_solve_request(model, "custom-id")
                self.assertEqual(request.request_id, "custom-id")
                self.assertEqual(request.model.sense, expected)
                self.assertIs(variables[0], model.x)
                var = request.model.variables[0]
                self.assertEqual(var.objective, 3)
                self.assertFalse(var.HasField("lower"))
                self.assertFalse(var.HasField("upper"))

    def test_variable_bounds_and_zero_objective_coefficients(self):
        model = self.model()
        model.x.setlb(-2)
        model.x.setub(5)
        model.y = pyo.Var(domain=pyo.NonNegativeReals)
        model.z = pyo.Var(bounds=(None, 9))
        request, variables = to_solve_request(model)
        self.assertEqual([v.name for v in variables], ["x", "y", "z"])
        x, y, z = request.model.variables
        self.assertEqual((x.lower, x.upper), (-2, 5))
        self.assertTrue(y.HasField("lower"))
        self.assertEqual(y.lower, 0)
        self.assertFalse(y.HasField("upper"))
        self.assertEqual(y.objective, 0)
        self.assertFalse(z.HasField("lower"))
        self.assertEqual(z.upper, 9)

    def test_constraint_bounds_and_constants(self):
        model = self.model()
        model.upper = pyo.Constraint(expr=2 * model.x + 3 <= 9)
        model.lower = pyo.Constraint(expr=3 * model.x + 4 >= 10)
        model.equality = pyo.Constraint(expr=model.x + 5 == 12)
        model.ranged = pyo.Constraint(expr=pyo.inequality(2, 4 * model.x + 6, 14))
        request, _ = to_solve_request(model)
        upper, lower, equality, ranged = request.model.constraints
        self.assertEqual(list(upper.indices), [0])
        self.assertEqual(list(upper.coefficients), [2])
        self.assertFalse(upper.HasField("lower"))
        self.assertEqual(upper.upper, 6)
        self.assertEqual(lower.lower, 6)
        self.assertFalse(lower.HasField("upper"))
        self.assertEqual((equality.lower, equality.upper), (7, 7))
        self.assertEqual((ranged.lower, ranged.upper), (-4, 8))

    def test_fixed_indexed_variables_and_objective_constant(self):
        model = pyo.ConcreteModel()
        model.x = pyo.Var([0, 1], initialize=2)
        model.x[0].fix()
        model.objective = pyo.Objective(expr=3 * model.x[0] + 4 * model.x[1] + 7)
        model.row = pyo.Constraint(expr=pyo.inequality(1, model.x[0] + 2 * model.x[1] + 5, 13))
        request, variables = to_solve_request(model)
        self.assertEqual([v.name for v in variables], ["x[1]"])
        self.assertEqual(request.model.variables[0].objective, 4)
        row = request.model.constraints[0]
        self.assertEqual(list(row.indices), [0])
        self.assertEqual(list(row.coefficients), [2])
        self.assertEqual((row.lower, row.upper), (-6, 6))
        variables[0].set_value(3)
        self.assertEqual(pyo.value(model.objective), 25)
        self.assertEqual(pyo.value(model.x[0]), 2)

    def test_nested_blocks_parameters_expressions_and_inactive_components(self):
        model = self.model()
        model.b = pyo.Block([1])
        model.b[1].y = pyo.Var()
        model.p = pyo.Param(initialize=2, mutable=True)
        model.expression = pyo.Expression(expr=model.p * model.b[1].y)
        model.row = pyo.Constraint(expr=model.x + model.expression <= 8)
        model.ignored = pyo.Constraint(expr=model.x**2 <= 4)
        model.ignored.deactivate()
        model.other_objective = pyo.Objective(expr=model.x**2)
        model.other_objective.deactivate()
        model.off = pyo.Block()
        model.off.integer = pyo.Var(domain=pyo.Integers)
        model.off.deactivate()
        for coefficient in (2, 5):
            model.p.set_value(coefficient)
            request, variables = to_solve_request(model)
            self.assertEqual([v.name for v in variables], ["x", "b[1].y"])
            self.assertEqual(len(request.model.constraints), 1)
            self.assertEqual(list(request.model.constraints[0].coefficients), [1, coefficient])

    def test_constant_constraint_preserves_infeasibility(self):
        model = self.model()
        model.fixed = pyo.Var(initialize=2)
        model.fixed.fix()
        model.row = pyo.Constraint(expr=model.fixed >= 3)
        request, _ = to_solve_request(model)
        row = request.model.constraints[0]
        self.assertEqual(list(row.indices), [])
        self.assertEqual(row.lower, 1)

    def test_requires_one_active_objective(self):
        model = self.model()
        model.objective.deactivate()
        with self.assertRaisesRegex(ValueError, "exactly one active objective"):
            to_solve_request(model)
        model.objective.activate()
        model.other = pyo.Objective(expr=model.x)
        with self.assertRaisesRegex(ValueError, "exactly one active objective"):
            to_solve_request(model)

    def test_requires_an_unfixed_variable(self):
        model = self.model()
        model.x.fix(2)
        with self.assertRaisesRegex(ValueError, "at least one unfixed variable"):
            to_solve_request(model)

    def test_rejects_integer_and_binary_domains(self):
        for domain in (pyo.Integers, pyo.Binary):
            with self.subTest(domain=domain):
                model = self.model()
                model.x.domain = domain
                with self.assertRaisesRegex(ValueError, "must be continuous"):
                    to_solve_request(model)

    def test_rejects_nonlinear_objective_and_constraint(self):
        model = self.model()
        for expression in (model.x**3, pyo.sin(model.x)):
            with self.subTest(expression=str(expression)):
                model.objective.set_value(expression)
                with self.assertRaisesRegex(ValueError, "objective must be linear or quadratic"):
                    to_solve_request(model)
        model.objective.set_value(model.x)
        model.row = pyo.Constraint(expr=model.x**2 <= 1)
        with self.assertRaisesRegex(ValueError, "row must be linear"):
            to_solve_request(model)

    def test_quadratic_objective_merges_pairs_and_substitutes_fixed_variables(self):
        model = pyo.ConcreteModel()
        model.x = pyo.Var()
        model.y = pyo.Var()
        model.fixed = pyo.Var(initialize=2)
        model.fixed.fix()
        model.objective = pyo.Objective(
            expr=model.y**2 + 3 * model.x * model.y + model.y * model.x + model.fixed * model.x + model.fixed**2
        )
        prepared = prepare_model(model, "quadratic")
        problem = prepared.request.model
        self.assertEqual(list(problem.quadratic_rows), [0, 1])
        self.assertEqual(list(problem.quadratic_columns), [1, 1])
        self.assertEqual(list(problem.quadratic_coefficients), [4, 1])
        self.assertEqual([v.objective for v in problem.variables], [2, 0])
        self.assertEqual(prepared.objective_constant, 4)

    def test_rejects_unsupported_components(self):
        for component in (pyo.SOSConstraint, Disjunct):
            with self.subTest(component=component):
                model = self.model()
                if component is pyo.SOSConstraint:
                    model.unsupported = component(rule=lambda m: [m.x], sos=1)
                else:
                    model.unsupported = component()
                with self.assertRaisesRegex(ValueError, "unsupported component unsupported"):
                    to_solve_request(model)

    def test_rejects_invalid_bounds_and_fixed_values(self):
        model = self.model()
        model.x.setlb(5)
        model.x.setub(1)
        with self.assertRaisesRegex(ValueError, "lower bound exceeds upper bound"):
            to_solve_request(model)
        model.x.setlb(0)
        model.x.fix(2, skip_validation=True)
        with self.assertRaisesRegex(ValueError, "fixed value is outside its bounds"):
            to_solve_request(model)

    def test_rejects_nonfinite_numbers(self):
        model = self.model()
        model.objective.set_value(float("inf") * model.x)
        with self.assertRaisesRegex(ValueError, "must be finite"):
            to_solve_request(model)

    def test_rejects_variables_outside_model(self):
        model = self.model()
        external = pyo.ConcreteModel()
        external.y = pyo.Var()
        model.row = pyo.Constraint(expr=model.x + external.y <= 5)
        with self.assertRaisesRegex(ValueError, "outside the active model"):
            to_solve_request(model)

    def test_request_id_uses_utf8_byte_limit(self):
        model = self.model()
        for request_id in ("", "é" * 65):
            with (
                self.subTest(request_id=request_id),
                self.assertRaisesRegex(ValueError, "1..128 UTF-8 bytes"),
            ):
                to_solve_request(model, request_id)

    def test_requires_constructed_model(self):
        with self.assertRaisesRegex(ValueError, "constructed Pyomo model"):
            to_solve_request(pyo.AbstractModel())


if __name__ == "__main__":
    unittest.main()
