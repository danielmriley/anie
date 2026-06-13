"""Shared plumbing for the SWE-bench Lite adapters.

Dataset loading (deterministic subset rule from
docs/external_benchmarks/README.md: first N of the test split sorted by
instance_id) plus the cached-clone / per-instance-worktree layout under
benchmarks/work/. Both the anie arm (run_anie.py) and the control arm
(run_control.py, PR3) import from here so the two arms cannot drift on
subset selection or checkout state.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

DATASET = "princeton-nlp/SWE-bench_Lite"
SPLIT = "test"

BENCHMARKS_DIR = Path(__file__).resolve().parent.parent
WORK_DIR = BENCHMARKS_DIR / "work"
REPOS_DIR = WORK_DIR / "repos"
INSTANCES_DIR = WORK_DIR / "instances"
METRICS_DIR = WORK_DIR / "metrics"
LOGS_DIR = WORK_DIR / "logs"
PREDICTIONS_DIR = WORK_DIR / "predictions"


def load_subset(limit: int) -> list[dict]:
    """First `limit` instances of the test split, sorted by instance_id."""
    from datasets import load_dataset  # deferred: import is slow

    ds = load_dataset(DATASET, split=SPLIT)
    rows = sorted(ds, key=lambda r: r["instance_id"])
    return rows[:limit]


def _git(*args: str, cwd: Path | None = None) -> str:
    result = subprocess.run(
        ["git", *args],
        cwd=cwd,
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout


def bare_clone_path(repo: str) -> Path:
    """e.g. 'astropy/astropy' -> benchmarks/work/repos/astropy__astropy.git"""
    return REPOS_DIR / (repo.replace("/", "__") + ".git")


def ensure_bare_clone(repo: str, commit: str) -> Path:
    """Cached bare clone of github.com/<repo>; fetch only if `commit` is missing."""
    bare = bare_clone_path(repo)
    if not bare.exists():
        REPOS_DIR.mkdir(parents=True, exist_ok=True)
        print(f"  cloning https://github.com/{repo} (bare, cached) ...")
        _git("clone", "--bare", f"https://github.com/{repo}.git", str(bare))
    if not _has_commit(bare, commit):
        print(f"  {repo}: commit {commit[:12]} not in cache, fetching ...")
        # Explicit refspec: --bare clones have no remote.origin.fetch config,
        # so a plain `git fetch origin` would only update the default branch.
        _git("fetch", "origin", "+refs/heads/*:refs/heads/*", cwd=bare)
        if not _has_commit(bare, commit):
            raise RuntimeError(f"{repo}: base_commit {commit} not found after fetch")
    return bare


def _has_commit(bare: Path, commit: str) -> bool:
    probe = subprocess.run(
        ["git", "cat-file", "-e", f"{commit}^{{commit}}"],
        cwd=bare,
        capture_output=True,
    )
    return probe.returncode == 0


def ensure_worktree(instance: dict) -> Path:
    """Detached worktree for one instance at its base_commit.

    Reused worktrees (crashed earlier run) are hard-reset and cleaned so
    every generation attempt starts from a pristine base_commit tree.
    """
    repo = instance["repo"]
    commit = instance["base_commit"]
    bare = ensure_bare_clone(repo, commit)
    worktree = INSTANCES_DIR / instance["instance_id"]
    if worktree.exists():
        _git("reset", "--hard", commit, cwd=worktree)
        _git("clean", "-fdx", cwd=worktree)
    else:
        INSTANCES_DIR.mkdir(parents=True, exist_ok=True)
        _git("worktree", "add", "--detach", str(worktree), commit, cwd=bare)
    return worktree


# Build artifacts an agent may create in the worktree while reproducing an
# issue (e.g. `python -m venv .venv && pip install -e .`). SWE-bench gold
# patches only ever touch repository source, and the official evaluator
# applies the patch to a clean checkout, so capturing these is never
# correct — and a stray virtualenv balloons one patch to >200k lines
# (observed 2026-06-12, django__django-11620: 1 real src edit + 576 venv
# files). Excluded at BOTH the git layer (pathspec, so they never stage)
# and the patch-text layer (filter_patch_artifacts, defense in depth).
_ARTIFACT_EXCLUDES = [
    ":(exclude).venv/**",
    ":(exclude)venv/**",
    ":(exclude)env/**",
    ":(exclude)**/__pycache__/**",
    ":(exclude)**/*.pyc",
    ":(exclude).pytest_cache/**",
    ":(exclude).tox/**",
    ":(exclude)**/*.egg-info/**",
    ":(exclude)node_modules/**",
]

# Top-level path prefixes that are never repository source. Used by the
# patch-text filter, which works on already-captured patches (the only
# remediation available once a worktree has been reset/reused).
_ARTIFACT_PREFIXES = (
    ".venv/",
    "venv/",
    "env/",
    "node_modules/",
)


def _is_artifact_path(path: str) -> bool:
    return (
        path.startswith(_ARTIFACT_PREFIXES)
        or path.startswith(("__pycache__/", ".pytest_cache/", ".tox/"))
        or "/__pycache__/" in path
        or path.endswith(".pyc")
        or ".egg-info/" in path
    )


def filter_patch_artifacts(patch: str) -> str:
    """Drop `diff --git` sections that touch build artifacts, not source.

    Defense in depth for `worktree_diff` and the remediation tool for
    patches captured before the pathspec exclusion existed: a unified
    diff is a concatenation of `diff --git a/<p> b/<p>` sections, so we
    keep only sections whose path is not an artifact.
    """
    if "diff --git " not in patch:
        return patch
    out: list[str] = []
    keep = True
    for line in patch.splitlines(keepends=True):
        if line.startswith("diff --git "):
            path = line.split(" b/", 1)[0][len("diff --git a/") :]
            keep = not _is_artifact_path(path)
        if keep:
            out.append(line)
    return "".join(out)


def worktree_diff(worktree: Path) -> str:
    """Patch of everything the agent changed, including untracked new files.

    Staging first (`git add -A`) is what pulls new files into the patch;
    plain `git diff` would miss them. This is the single capture rule for
    BOTH arms: the anie arm has no other deliverable, and the control arm
    deliberately ignores mini-swe-agent's curated `git diff -- <files>`
    submission so the two arms' predictions are produced under identical
    inclusion rules (see the capture-rule comment in run_control.py).

    Build artifacts (`.venv/`, `__pycache__/`, ...) are excluded via
    pathspec so they never stage, then filtered again from the patch text.
    """
    _git("add", "-A", "--", ".", *_ARTIFACT_EXCLUDES, cwd=worktree)
    patch = _git("diff", "--cached", "--", ".", *_ARTIFACT_EXCLUDES, cwd=worktree)
    return filter_patch_artifacts(patch)


def load_done_ids(predictions_path: Path) -> set[str]:
    """instance_ids already present in a predictions.jsonl (resume support)."""
    done: set[str] = set()
    if predictions_path.exists():
        for line in predictions_path.read_text().splitlines():
            line = line.strip()
            if line:
                done.add(json.loads(line)["instance_id"])
    return done


def append_prediction(predictions_path: Path, instance_id: str, model_name_or_path: str, model_patch: str) -> None:
    predictions_path.parent.mkdir(parents=True, exist_ok=True)
    row = {
        "instance_id": instance_id,
        "model_name_or_path": model_name_or_path,
        "model_patch": model_patch,
    }
    with predictions_path.open("a") as f:
        f.write(json.dumps(row) + "\n")


def sanitize(name: str) -> str:
    """Model/mode strings -> filesystem- and schema-safe token."""
    return name.replace("/", "-").replace(":", "-")
