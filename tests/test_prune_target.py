import os
import subprocess
from pathlib import Path

SCRIPT = Path(__file__).parents[1] / "scripts" / "prune-target.sh"


def _tool(path: Path, body: str) -> None:
    path.write_text("#!/bin/sh\n" + body)
    path.chmod(0o755)


def _run(tmp_path: Path, *, size=1, free="2000000", du_status=0, lsof_status=1, args=(), marker=True):
    tools = tmp_path / "tools"
    tools.mkdir()
    target = tmp_path / "cache" / "target"
    target.mkdir(parents=True)
    if marker:
        (target / "CACHEDIR.TAG").write_text("Signature: 8a477f597d28d172789f06886806bc55\n")
    _tool(tools / "du", f"[ {du_status} -eq 0 ] || exit {du_status}\nprintf '{size}\\t%s\\n' \"$3\"\n")
    _tool(tools / "df", f"printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\nmock 1 1 {free} 1%% /\\n'\n")
    _tool(tools / "lsof", f"[ {lsof_status} -eq 0 ] && printf 'COMMAND PID\\ncargo 1\\n'; exit {lsof_status}\n")
    _tool(tools / "cargo", "printf '%s\\n' \"$*\" >> \"$CARGO_LOG\"\n")
    env = os.environ | {
        "PATH": f"{tools}:{os.environ['PATH']}",
        "CARGO_LOG": str(tmp_path / "cargo.log"),
        "CARGO_TARGET_DIR": str(target),
        "DISTILLPDF_TARGET_MAX_GIB": "1",
        "DISTILLPDF_MIN_FREE_GIB": "1",
    }
    return subprocess.run([SCRIPT, *args], env=env, text=True, capture_output=True), target


def test_lean_tree_is_noop(tmp_path: Path) -> None:
    result, _ = _run(tmp_path)
    assert result.returncode == 0
    assert not (tmp_path / "cargo.log").exists()


def test_oversize_requires_explicit_execution(tmp_path: Path) -> None:
    result, _ = _run(tmp_path, size=2_000_000)
    assert result.returncode == 3
    assert "--execute" in result.stderr


def test_busy_target_is_refused(tmp_path: Path) -> None:
    result, _ = _run(tmp_path, size=2_000_000, lsof_status=0, args=("--execute",))
    assert result.returncode == 4
    assert not (tmp_path / "cargo.log").exists()


def test_probe_failure_is_refused(tmp_path: Path) -> None:
    result, _ = _run(tmp_path, size=2_000_000, lsof_status=2, args=("--execute",))
    assert result.returncode == 4
    assert not (tmp_path / "cargo.log").exists()


def test_missing_size_probe_is_refused(tmp_path: Path) -> None:
    result, _ = _run(tmp_path, du_status=2, args=("--execute",))
    assert result.returncode == 1
    assert "probe unavailable" in result.stderr
    assert not (tmp_path / "cargo.log").exists()


def test_empty_free_space_probe_is_refused(tmp_path: Path) -> None:
    result, _ = _run(tmp_path, free="", args=("--execute",))
    assert result.returncode == 1
    assert "invalid free-space" in result.stderr
    assert not (tmp_path / "cargo.log").exists()


def test_unmarked_target_is_refused(tmp_path: Path) -> None:
    result, _ = _run(tmp_path, size=2_000_000, marker=False, args=("--execute",))
    assert result.returncode == 2
    assert "unmarked cache" in result.stderr
    assert not (tmp_path / "cargo.log").exists()


def test_repository_ancestor_is_refused(tmp_path: Path) -> None:
    env = os.environ | {"CARGO_TARGET_DIR": str(SCRIPT.parents[1].parent)}
    result = subprocess.run([SCRIPT, "--execute"], env=env, text=True, capture_output=True)
    assert result.returncode == 2
    assert "contains the repository" in result.stderr


def test_execute_cleans_only_resolved_target(tmp_path: Path) -> None:
    result, target = _run(tmp_path, size=2_000_000, args=("--execute",))
    assert result.returncode == 0
    assert (tmp_path / "cargo.log").read_text() == f"clean --target-dir {target}\n"


def test_invalid_option_never_cleans(tmp_path: Path) -> None:
    result, _ = _run(tmp_path, args=("--execute", "extra"))
    assert result.returncode == 2
    assert not (tmp_path / "cargo.log").exists()
