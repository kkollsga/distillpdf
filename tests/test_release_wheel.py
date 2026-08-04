import os
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "scripts" / "release-wheel.sh"


def test_release_wheel_is_bound_to_this_maturin_run(tmp_path: Path) -> None:
    """A newer shared wheel must not replace the artifact this run produced."""
    shared = tmp_path / "target" / "wheels"
    shared.mkdir(parents=True)
    built_in_shared = shared / "distillpdf-0.0.34-cp38-abi3-old.whl"
    unrelated = shared / "distillpdf-9.9.9-cp38-abi3-concurrent.whl"
    built_in_shared.write_bytes(b"the wheel an old release-check run meant to test")
    unrelated.write_bytes(b"unrelated concurrent build")
    os.utime(built_in_shared, (1, 1))
    os.utime(unrelated, (2, 2))

    # This is the old `ls -t target/wheels/*.whl | head -1` failure mode.
    assert max(shared.glob("distillpdf-*-abi3-*.whl"), key=lambda p: p.stat().st_mtime) == unrelated

    output_dir = tmp_path / "this-run"
    output_dir.mkdir()
    fake_maturin = tmp_path / "maturin"
    args_log = tmp_path / "maturin-args"
    fake_maturin.write_text(
        """#!/usr/bin/env bash
set -euo pipefail
printf '%s\\n' \"$@\" > \"$FAKE_MATURIN_ARGS\"
while [ \"$#\" -gt 0 ]; do
  if [ \"$1\" = --out ]; then
    output_dir=\"$2\"
    shift 2
  else
    shift
  fi
done
printf 'this run only' > \"$output_dir/distillpdf-0.0.34-cp38-abi3-test.whl\"
""",
        encoding="utf-8",
    )
    fake_maturin.chmod(0o755)
    build_python = "/explicit/build-python3.12"
    env = os.environ | {"FAKE_MATURIN_ARGS": str(args_log)}

    completed = subprocess.run(
        [
            "bash",
            "-c",
            'source "$1"; build_release_wheel "$2" "$3" "$4"',
            "bash",
            str(HELPER),
            str(fake_maturin),
            build_python,
            str(output_dir),
        ],
        check=True,
        capture_output=True,
        text=True,
        env=env,
    )

    wheel = Path(completed.stdout.strip())
    assert wheel == output_dir / "distillpdf-0.0.34-cp38-abi3-test.whl"
    assert wheel.read_bytes() == b"this run only"
    assert unrelated.read_bytes() == b"unrelated concurrent build"
    assert args_log.read_text(encoding="utf-8").splitlines() == [
        "build",
        "--release",
        "--features",
        "extension-module",
        "-i",
        build_python,
        "--out",
        str(output_dir),
    ]
