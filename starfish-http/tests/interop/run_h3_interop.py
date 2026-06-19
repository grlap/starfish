#!/usr/bin/env python3
"""Run the HTTP/3 interop lanes against external stacks."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
from pathlib import Path


def resolve_executable(name_or_path: str) -> str | None:
    candidate = Path(name_or_path)
    if candidate.parent != Path(""):
        return str(candidate) if candidate.exists() else None
    return shutil.which(name_or_path)


def run(repo_root: Path, *args: str) -> None:
    print("+", " ".join(args), flush=True)
    subprocess.run(args, cwd=repo_root, check=True)


def ensure_quiche_dependencies() -> None:
    client = os.environ.get("STARFISH_QUICHE_CLIENT", "quiche-client")
    server = os.environ.get("STARFISH_QUICHE_SERVER", "quiche-server")
    missing = []
    if resolve_executable(client) is None:
        missing.append(
            f"missing quiche client executable: {client!r} (set STARFISH_QUICHE_CLIENT if needed)"
        )
    if resolve_executable(server) is None:
        missing.append(
            f"missing quiche server executable: {server!r} (set STARFISH_QUICHE_SERVER if needed)"
        )
    if missing:
        raise SystemExit("\n".join(missing))


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Run the starfish-http HTTP/3 interop tests.",
    )
    parser.add_argument(
        "--lane",
        choices=("aioquic", "quiche", "all"),
        default="aioquic",
        help="Which external interop lane(s) to run.",
    )
    parser.add_argument(
        "--bootstrap-aioquic",
        action="store_true",
        help="Allow the aioquic lane to bootstrap a cached virtualenv if needed.",
    )
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parents[3]

    try:
        if args.lane in {"aioquic", "all"}:
            env = os.environ.copy()
            if args.bootstrap_aioquic:
                env["STARFISH_AIOQUIC_BOOTSTRAP"] = "1"
            run(
                repo_root,
                "cargo",
                "test",
                "-p",
                "starfish-http",
                "--test",
                "h3_interop_aioquic",
                env=env,
            )

        if args.lane in {"quiche", "all"}:
            ensure_quiche_dependencies()
            run(
                repo_root,
                "cargo",
                "test",
                "-p",
                "starfish-http",
                "--test",
                "h3_interop_quiche",
            )
    except subprocess.CalledProcessError as exc:
        return exc.returncode

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
