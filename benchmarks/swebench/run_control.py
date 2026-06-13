#!/usr/bin/env python3
"""SWE-bench Lite control arm: mini-swe-agent (docs/external_benchmarks/01, PR3).

Same deterministic subset, same Ollama model, same wall-clock budget as the
anie arm (run_anie.py), but driven by mini-swe-agent — the ~100-line
reference scaffold from the SWE-bench team. anie-vs-control on identical
instances is the primary claim of the external-benchmarks series; this
script produces the control side of that comparison.

mini-swe-agent's own batch runner (mini-extra swebench) requires Docker
images, which this machine doesn't have. We instead drive its DefaultAgent
as a library with its LocalEnvironment pointed at the same git worktrees
common.py prepares for the anie arm, using the prompts from the installed
package's official swebench config verbatim (only the docker-ism `/testbed`
is rewritten to the worktree path). Both arms therefore see the same
checkout state, the same subset rule, and the same kill semantics.

Per instance the parent forks a child (`--child <spec.json>`, internal) and
SIGKILLs the whole process group at the budget — identical to run_anie.py.
The prediction is always the worktree diff at the deadline, captured by the
same common.worktree_diff() call run_anie.py uses — NOT the agent's curated
submission (see the capture-rule comment in run_one), so both arms produce
patches under identical inclusion rules and budget truncation is treated
symmetrically.

Resumable: instance_ids already present in --out are skipped.
"""

from __future__ import annotations

import argparse
import json
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

import common


def mini_version() -> str | None:
    import importlib.metadata

    try:
        return importlib.metadata.version("mini-swe-agent")
    except importlib.metadata.PackageNotFoundError:
        return None


def ensure_mini_installed() -> str:
    """pip-install mini-swe-agent into the venv if missing; return its version."""
    version = mini_version()
    if version is not None:
        return version
    venv = common.BENCHMARKS_DIR / ".venv"
    if venv not in Path(sys.executable).parents:
        sys.exit(
            f"error: mini-swe-agent is not installed and {sys.executable} is not "
            f"the benchmarks venv ({venv}/bin/python) — refusing to pip install "
            "outside the venv. Re-run under the venv python."
        )
    print("installing mini-swe-agent into benchmarks/.venv ...")
    subprocess.run([sys.executable, "-m", "pip", "install", "mini-swe-agent"], check=True)
    version = mini_version()
    if version is None:
        sys.exit("error: mini-swe-agent still not importable after pip install")
    return version


# ---------------------------------------------------------------------------
# Child: one instance, in-process mini-swe-agent run. Forked so the parent's
# budget SIGKILL takes out litellm requests and the agent's bash subprocesses
# exactly like it takes out anie's process group in run_anie.py.
# ---------------------------------------------------------------------------


def run_child(spec_path: str) -> int:
    spec = json.loads(Path(spec_path).read_text())
    os.environ.setdefault("MSWEA_SILENT_STARTUP", "1")

    import yaml
    from minisweagent.agents.default import DefaultAgent
    from minisweagent.config import builtin_config_dir
    from minisweagent.environments.local import LocalEnvironment
    from minisweagent.models.litellm_model import LitellmModel

    # The installed package's official SWE-bench prompts/templates, verbatim
    # except for one local-environment fix-up: the templates hardcode the
    # docker image's /testbed working directory, which here is the worktree.
    cfg = yaml.safe_load((builtin_config_dir / "benchmarks" / "swebench.yaml").read_text())
    agent_cfg = cfg["agent"]
    for key in ("system_template", "instance_template"):
        agent_cfg[key] = agent_cfg[key].replace("/testbed", spec["worktree"])

    model = LitellmModel(
        model_name=spec["litellm_model"],
        model_kwargs=cfg["model"].get("model_kwargs", {}),
        observation_template=cfg["model"]["observation_template"],
        format_error_template=cfg["model"]["format_error_template"],
        # Local models have no litellm price entry; without this the first
        # cost calculation raises. Cost stays 0.0, so the config's cost_limit
        # never binds — wall-clock is the budget, same as the anie arm.
        cost_tracking="ignore_errors",
    )

    env_cfg = cfg.get("environment", {})
    # Drop BASH_ENV=/root/.bashrc (a docker-image conda hook); keep the
    # benign pager/progress-bar settings.
    env_vars = {k: v for k, v in env_cfg.get("env", {}).items() if k != "BASH_ENV"}
    env = LocalEnvironment(
        cwd=spec["worktree"],
        timeout=env_cfg.get("timeout", 60),
        env=env_vars,
    )

    agent = DefaultAgent(
        model,
        env,
        system_template=agent_cfg["system_template"],
        instance_template=agent_cfg["instance_template"],
        step_limit=agent_cfg.get("step_limit", 250),
        cost_limit=agent_cfg.get("cost_limit", 3.0),
        # Belt to the parent's SIGKILL braces: lets the agent exit cleanly
        # (and persist its trajectory) if a step ends near the deadline.
        wall_time_limit_seconds=spec["budget_s"],
        output_path=Path(spec["traj_path"]),
    )

    try:
        info = agent.run(spec["problem_statement"])
        exit_status = info.get("exit_status", "")
        submission = info.get("submission", "") or ""
    except Exception as e:  # trajectory already records the traceback
        exit_status, submission = type(e).__name__, ""
    Path(spec["result_path"]).write_text(
        json.dumps({"exit_status": exit_status, "submission": submission})
    )
    return 0


# ---------------------------------------------------------------------------
# Parent: subset loop, budget enforcement, predictions + metrics sidecars.
# ---------------------------------------------------------------------------


def metrics_from_trajectory(traj_path: Path) -> dict:
    """Token/turn/tool counters from a mini-swe-agent trajectory file.

    Shape mirrors the anie RunMetrics paths harvest.py reads (tokens.*,
    turns, tools.calls/failures) so the two arms join identically. The
    trajectory is saved after every step, so a SIGKILLed run still yields
    counters for the steps it completed; a missing/torn file yields zeros.
    """
    counters = {
        "turns": 0,
        "tokens": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0},
        "tools": {"calls": 0, "failures": 0},
        "format_errors": 0,
    }
    try:
        traj = json.loads(traj_path.read_text())
    except (OSError, json.JSONDecodeError):
        return counters
    counters["turns"] = traj.get("info", {}).get("model_stats", {}).get("api_calls", 0)
    for msg in traj.get("messages", []):
        extra = msg.get("extra", {})
        # Exactly one persisted response per model call: on the assistant
        # message for a parseable reply, on the FormatError message otherwise.
        usage = (extra.get("response") or {}).get("usage") or {}
        counters["tokens"]["input_tokens"] += usage.get("prompt_tokens") or 0
        counters["tokens"]["output_tokens"] += usage.get("completion_tokens") or 0
        if msg.get("role") == "assistant":
            counters["tools"]["calls"] += len(extra.get("actions", []))
        elif extra.get("interrupt_type") == "FormatError":
            counters["format_errors"] += 1
        elif "returncode" in extra:  # observation message for one executed action
            if extra["returncode"] not in (0, None):
                counters["tools"]["failures"] += 1
    counters["tokens"]["total_tokens"] = (
        counters["tokens"]["input_tokens"] + counters["tokens"]["output_tokens"]
    )
    return counters


def run_one(instance: dict, args: argparse.Namespace, run_name: str) -> tuple[str, float, str]:
    """Run mini-swe-agent on one instance. Returns (model_patch, wall_secs, status)."""
    iid = instance["instance_id"]
    worktree = common.ensure_worktree(instance)

    log_dir = common.LOGS_DIR / run_name
    log_dir.mkdir(parents=True, exist_ok=True)
    spec_path = log_dir / f"{iid}.spec.json"
    traj_path = log_dir / f"{iid}.traj.json"
    result_path = log_dir / f"{iid}.result.json"
    log_path = log_dir / f"{iid}.log"
    traj_path.unlink(missing_ok=True)
    result_path.unlink(missing_ok=True)

    spec_path.write_text(
        json.dumps(
            {
                "worktree": str(worktree),
                "problem_statement": instance["problem_statement"],
                "litellm_model": args.litellm_model,
                "budget_s": args.budget_s,
                "traj_path": str(traj_path),
                "result_path": str(result_path),
            }
        )
    )

    cmd = [sys.executable, str(Path(__file__).resolve()), "--child", str(spec_path)]
    started = time.monotonic()
    status = "timeout"
    with log_path.open("w") as log:
        # New session so the budget kill takes out litellm + the agent's
        # bash subprocesses too — same semantics as the anie arm.
        proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            proc.wait(timeout=args.budget_s)
            status = "ok" if proc.returncode == 0 else f"exit={proc.returncode}"
        except subprocess.TimeoutExpired:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass  # exited in the window between wait() timing out and the kill
            proc.wait()
    wall = time.monotonic() - started

    exit_status = status
    if result_path.exists():
        result = json.loads(result_path.read_text())
        exit_status = result.get("exit_status") or status

    # Capture rule, shared with the anie arm: the worktree diff at the
    # deadline, via the same common.worktree_diff() call run_anie.py uses.
    # mini-swe-agent's official deliverable is its curated submission — a
    # `git diff -- <files>` over only the source files the agent chose to
    # list, with test/helper files explicitly excluded by the template. We
    # deliberately ignore it (it's still recorded in <iid>.result.json):
    # the anie arm has no curation step, so grading a curated control
    # patch against anie's raw worktree diff would compare the arms under
    # systematically different inclusion rules. The cost is symmetric
    # noise — e.g. mini's own patch.txt scratch file lands in the diff,
    # just as anie's stray reproduce scripts do.
    patch = common.worktree_diff(worktree)

    metrics_dir = common.METRICS_DIR / run_name
    metrics_dir.mkdir(parents=True, exist_ok=True)
    sidecar = {
        "schema": "swebench-control-arm-v1",
        "harness": "mini-swe-agent",
        "mini_swe_agent_version": args.mini_version,
        "model": args.litellm_model,
        "exit_status": exit_status,
        "wall_clock_ms": int(wall * 1000),
        **metrics_from_trajectory(traj_path),
    }
    (metrics_dir / f"{iid}.metrics.json").write_text(json.dumps(sidecar, indent=1))

    return patch, wall, exit_status


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Generate SWE-bench Lite predictions with mini-swe-agent, the "
        "same-model control arm (deterministic subset: first N of the test split "
        "by instance_id — identical to run_anie.py).",
    )
    parser.add_argument("--limit", type=int, default=25, help="subset size (default: 25)")
    parser.add_argument("--model", default="gemma4:e4b", help="Ollama model id (default: gemma4:e4b)")
    parser.add_argument(
        "--litellm-model",
        default=None,
        # ollama_chat/ (not ollama/): both target the local Ollama server, but
        # litellm's plain ollama/ provider doesn't support the `tools` param,
        # and mini-swe-agent's drop_params would silently strip its bash tool
        # — every step would be a format error by wiring, not by model.
        # ollama_chat/ uses /api/chat with native tool-call support.
        help="full litellm model id (default: ollama_chat/<model>)",
    )
    parser.add_argument("--budget-s", type=int, default=480, help="wall-clock kill per instance, seconds (default: 480)")
    parser.add_argument("--out", type=Path, default=None, help="predictions.jsonl path (default: benchmarks/work/predictions/mini-swe-agent__<model>.jsonl)")
    parser.add_argument("--child", metavar="SPEC_JSON", default=None, help=argparse.SUPPRESS)
    args = parser.parse_args()

    if args.child:
        return run_child(args.child)

    args.mini_version = ensure_mini_installed()
    if args.litellm_model is None:
        args.litellm_model = f"ollama_chat/{args.model}"

    model_name = f"mini-swe-agent__{common.sanitize(args.model)}"
    out = args.out or common.PREDICTIONS_DIR / f"{model_name}.jsonl"
    run_name = out.stem

    # Provenance for the eventual report: the exact scaffold version that
    # produced this predictions file.
    out.parent.mkdir(parents=True, exist_ok=True)
    meta = {
        "harness": "mini-swe-agent",
        "mini_swe_agent_version": args.mini_version,
        "litellm_model": args.litellm_model,
        "budget_s": args.budget_s,
        "subset": f"first {args.limit} of {common.DATASET} [{common.SPLIT}] by instance_id",
    }
    out.with_suffix(".meta.json").write_text(json.dumps(meta, indent=1) + "\n")

    instances = common.load_subset(args.limit)
    done = common.load_done_ids(out)
    todo = [i for i in instances if i["instance_id"] not in done]
    print(f"control arm: mini-swe-agent {args.mini_version}, model {args.litellm_model}")
    print(f"subset: first {args.limit} of {common.DATASET} [{common.SPLIT}] by instance_id")
    print(f"out: {out} ({len(done)} already done, {len(todo)} to run)")

    for n, instance in enumerate(todo, 1):
        iid = instance["instance_id"]
        print(f"[{n}/{len(todo)}] {iid} ...", flush=True)
        try:
            patch, wall, status = run_one(instance, args, run_name)
        except subprocess.CalledProcessError as e:
            print(f"  git failure: {e.stderr.strip() if e.stderr else e}", file=sys.stderr)
            print("  skipping (not recorded; will retry on resume)", file=sys.stderr)
            continue
        common.append_prediction(out, iid, model_name, patch)
        print(f"  {status}, {wall:.0f}s, patch {len(patch)} bytes")

    print(f"done: {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
