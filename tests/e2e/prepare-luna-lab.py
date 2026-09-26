#!/usr/bin/env python3
"""Prepare, but do not start, a disposable Hel lab for tmux exploration."""

from __future__ import annotations

import argparse
import json
import pathlib
import shlex

from reliability_lab import Lab


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--seed", required=True, type=int)
    parser.add_argument("--hel", required=True, type=pathlib.Path)
    parser.add_argument("--fake-delay-ms", type=int, default=15000)
    args = parser.parse_args()
    if args.fake_delay_ms < 0:
        parser.error("--fake-delay-ms must be non-negative")

    # The lab outlives this script by design, so it gets no watchdog;
    # finish-luna-lab.py cleans it up when the campaign ends.
    lab = Lab(args.hel, "luna-manual", args.seed, watchdog=False)
    # Manual labs advertise a fixed port before the operator starts the daemon.
    port = lab.prepare(fake_acp_delay_ms=args.fake_delay_ms, phone_port=Lab.free_port())
    values = {
        "MJ_CONFIG_DIR": str(lab.config),
        "MJ_DATA_DIR": str(lab.data),
        "MJ_CHAOS_ISOLATED": "1",
        "RUST_LOG": "hel=debug,mj=debug,mj_controller=debug",
        "MJ_LUNA_ARTIFACTS": str(lab.root),
        "MJ_LUNA_RUNTIME_ROOT": str(lab.runtime_root),
        "MJ_LUNA_PORT": str(port),
        "MJ_LUNA_BINARY": str(lab.hel),
        "MJ_LUNA_FAKE_ACP_DELAY_MS": str(args.fake_delay_ms),
    }
    environment_file = lab.root / "luna-env.sh"
    environment_file.write_text(
        "\n".join(f"export {key}={shlex.quote(value)}" for key, value in values.items()) + "\n"
    )
    (lab.root / "luna-lab.json").write_text(
        json.dumps({"seed": args.seed, "port": port, **values}, indent=2, sort_keys=True) + "\n"
    )
    print(f"artifacts={lab.root}")
    print(f"runtime={lab.runtime_root}")
    print(f"source {shlex.quote(str(environment_file))}")
    finish = pathlib.Path(__file__).resolve().parent / "finish-luna-lab.py"
    print(f"finish: python3 {shlex.quote(str(finish))} {shlex.quote(str(lab.root))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
