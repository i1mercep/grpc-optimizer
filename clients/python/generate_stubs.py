"""Regenerate the SDK's vendored bindings from the unchanged wire contract."""

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
OUTPUT = Path(__file__).parent / "grpc_optimizer" / "_generated"


def main() -> None:
    OUTPUT.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [
            sys.executable,
            "-m",
            "grpc_tools.protoc",
            "-I",
            str(ROOT / "proto"),
            f"--python_out={OUTPUT}",
            f"--pyi_out={OUTPUT}",
            f"--grpc_python_out={OUTPUT}",
            str(ROOT / "proto/optimization/v1/optimizer.proto"),
        ],
        check=True,
    )
    # Keep bindings private to the SDK without modifying the protobuf package
    # or adding generated directories to the interpreter's global import path.
    package = OUTPUT / "optimization" / "v1"
    grpc_module = package / "optimizer_pb2_grpc.py"
    grpc_module.write_text(
        grpc_module.read_text().replace(
            "from optimization.v1 import optimizer_pb2",
            "from grpc_optimizer._generated.optimization.v1 import optimizer_pb2",
        )
    )
    module = package / "optimizer_pb2.py"
    module.write_text(
        module.read_text().replace(
            "'optimization.v1.optimizer_pb2'", "'grpc_optimizer._generated.optimization.v1.optimizer_pb2'"
        )
    )
    for directory in (OUTPUT, OUTPUT / "optimization", package):
        (directory / "__init__.py").touch()


if __name__ == "__main__":
    main()
