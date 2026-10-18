#!/usr/bin/env python3
"""Exercise the built macOS CLI and its real subprocess supervisor.

Run with: python3 tests/local_project_smoke.py /absolute/path/to/telete
Only temporary fixture directories are changed. Failures retain their fixture.
"""

import argparse
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def run(argv, *, cwd, env, succeeds=True):
    result = subprocess.run(
        [str(argument) for argument in argv],
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    if (result.returncode == 0) != succeeds:
        raise AssertionError(
            f"Unexpected exit {result.returncode}: {argv!r}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def exercise(binary, scratch):
    home = scratch / "home"
    repo = scratch / "project"
    home.mkdir(mode=0o700)
    repo.mkdir(mode=0o700)
    state = repo / ".telete"
    env = {
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "HOME": str(home),
        "LANG": "C",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_AUTHOR_NAME": "Telete smoke",
        "GIT_AUTHOR_EMAIL": "smoke@example.invalid",
        "GIT_COMMITTER_NAME": "Telete smoke",
        "GIT_COMMITTER_EMAIL": "smoke@example.invalid",
    }

    def git(*args):
        return run(
            ["/usr/bin/git", "-c", "core.hooksPath=/dev/null",
             "-c", "commit.gpgsign=false", *args],
            cwd=repo, env=env,
        ).stdout.strip()

    def cli(*args, selected=state, succeeds=True):
        result = run(
            [binary, "--json", "--state", selected, *args],
            cwd=repo, env=env, succeeds=succeeds,
        )
        return json.loads(result.stdout) if succeeds else result

    # An observation must not create a queue or mutate a foreign journal.
    missing = scratch / "missing"
    cli("status", selected=missing, succeeds=False)
    require(not missing.exists(), "status created missing state")
    foreign = scratch / "foreign"
    foreign.mkdir(mode=0o700)
    database = foreign / "queue.sqlite3"
    with sqlite3.connect(database) as connection:
        connection.execute("CREATE TABLE unrelated (value TEXT)")
    database.chmod(0o600)
    before = database.read_bytes()
    cli("status", selected=foreign, succeeds=False)
    require(database.read_bytes() == before, "status changed a foreign journal")
    require(list(foreign.iterdir()) == [database], "status added foreign state files")

    git("init", "--quiet", "--initial-branch=main")
    (repo / ".gitignore").write_text(".telete/\n")
    (repo / "payload.txt").write_text("committed source\n")
    check = repo / "check.sh"
    check.write_text(
        "#!/bin/sh\nset -eu\n"
        "[ \"$1\" = 'literal argument' ]\n"
        "[ \"$PROJECT_MODE\" = frozen ]\n"
        "[ \"$(cat payload.txt)\" = 'committed source' ]\n"
        "[ ! -e caller-only.txt ]\n"
        "printf 'CARGO_HOME=%s\\n' \"$CARGO_HOME\"\n"
        "printf 'CARGO_TARGET_DIR=%s\\n' \"$CARGO_TARGET_DIR\"\n"
        "printf 'TMPDIR=%s\\n' \"$TMPDIR\"\n"
        "printf 'XDG_CACHE_HOME=%s\\n' \"$XDG_CACHE_HOME\"\n"
        "printf 'CHECKOUT=%s\\n' \"$PWD\"\n"
        "printf 'CONTEXT=%s\\n' \"$TELETE_CONTEXT\"\n"
        "printf 'MODE=%s\\n' \"$PROJECT_MODE\"\n"
        "printf 'CHECK_WRITE=ok\\n' > \"$CARGO_TARGET_DIR/smoke-output\"\n"
        "if (printf forbidden > \"$PROJECT_OUTSIDE\") 2>/dev/null; then\n"
        "  printf 'sandbox allowed a write outside selected state\\n' >&2\n"
        "  exit 93\nfi\n"
        "exit 0\n"
    )
    check.chmod(0o755)
    workflow_path = repo / "workflow.json"
    selection = {
        "check": {
            "program": "check.sh",
            "args": ["literal argument"],
            "env": {
                "PROJECT_MODE": "frozen",
                "PROJECT_OUTSIDE": str(scratch / "forbidden-output"),
            },
            "timeout_seconds": 10,
        }
    }
    workflow_path.write_text(json.dumps(selection))
    git("add", ".")
    git("commit", "--quiet", "-m", "Passing committed check")
    passing_source = git("rev-parse", "HEAD")

    initialized = cli("init", "--project", "--repo", repo)
    require(initialized["paused"], "a new queue was not paused")
    require(initialized["config"]["project"], "queue is not a project queue")
    require(initialized["config"]["workspace"] == str(state), "local binding differs")
    cli("resume")
    passing = cli("submit", passing_source, "--repo", repo, "--workflow", workflow_path)
    require(passing["phase"] == "queued", "submit ran a job synchronously")

    # Uncommitted files and edits to the selected workflow cannot alter admitted work.
    check.write_text("#!/bin/sh\nexit 91\n")
    (repo / "payload.txt").write_text("dirty caller source\n")
    (repo / "caller-only.txt").write_text("untracked\n")
    workflow_path.write_text(json.dumps({"check": {"program": "/usr/bin/false"}}))

    def complete(job_id):
        observed = cli("status", job_id)
        phases = []
        for _ in range(12):
            if observed["phase"] in {"succeeded", "failed", "cancelled", "already_included", "blocked"}:
                break
            phases.append(observed["phase"])
            cli("worker", "--once")
            observed = cli("status", job_id)
        require(observed["phase"] != "blocked", f"job blocked: {observed}")
        require(observed["phase"] in {"succeeded", "failed"}, f"job did not finish: {observed}")
        require("checking" in phases, "worker --once unexpectedly ran the whole lifecycle")
        return observed

    passed = complete(passing["id"])
    require(passed["phase"] == "succeeded", f"committed check failed: {passed}")
    require(passed["options"]["workflow"]["check"] == selection["check"], "workflow was not frozen")
    require(passed["candidate"] == passing_source, "check used another source")
    require(not passed["accepted"], "check-only workflow accepted source")
    stage = passed["workflow_runs"][0]
    require(stage["result"]["exit_code"] == 0, "check has no successful process result")
    require(stage["command"]["confined"], "real check did not request macOS confinement")
    observed_environment = dict(line.split("=", 1) for line in stage["result"]["stdout"].splitlines())
    for name, expected in {
        "CARGO_HOME": state / "cargo",
        "CARGO_TARGET_DIR": state / "targets",
        "TMPDIR": state / "tmp",
        "XDG_CACHE_HOME": state / "cache",
        "CHECKOUT": state / "jobs" / passed["id"] / "worktree",
    }.items():
        require(observed_environment[name] == str(expected), f"incorrect supervised {name}")
    context = Path(observed_environment["CONTEXT"])
    require(context.is_relative_to(state), "context escaped selected state")
    require(json.loads(context.read_text())["source"]["candidate"] == passing_source, "incorrect context source")
    require(not (scratch / "forbidden-output").exists(), "check escaped selected write boundary")
    require((state / "targets/smoke-output").read_text() == "CHECK_WRITE=ok\n", "check could not write build output")
    process_directory = state / "broker" / f"{passed['id']}-{stage['operation']}.process"
    for name in ["supervisor.json", "started.json", "result.json"]:
        require((process_directory / name).is_file(), f"real supervisor did not retain {name}")

    # Keep the workflow defaults: no --no-repair or --no-notify bypass is used.
    def no_cell_authority(job):
        require(not job["options"]["repair"] and not job["options"]["notify"], "provider stages opted in")
        require(job["policy"].get("signing_policy") is None, "Cell signing was frozen")
        require(all(value is None for value in job["policy"]["providers"].values()), "provider authority selected")
        require(job["policy"]["prompts"] is None, "repair prompts selected")
        require(not job["attempts"] and job["notification"] is None, "unexpected provider work")

    no_cell_authority(passed)
    git("checkout", "--", "check.sh", "payload.txt", "workflow.json")
    (repo / "caller-only.txt").unlink()
    check.write_text("#!/bin/sh\nprintf 'expected committed failure\\n' >&2\nexit 23\n")
    git("add", "check.sh")
    git("commit", "--quiet", "-m", "Failing committed check")
    failing_source = git("rev-parse", "HEAD")
    failing = cli("submit", failing_source, "--repo", repo, "--workflow", workflow_path)
    failed = complete(failing["id"])
    require(failed["phase"] == "failed", "failing committed check succeeded")
    require(failed["workflow_runs"][0]["result"]["exit_code"] == 23, "failure exit was lost")
    no_cell_authority(failed)
    require(cli("status", passed["id"])["phase"] == "succeeded", "passing outcome was not retained")
    require(cli("status", failed["id"])["phase"] == "failed", "failing outcome was not retained")
    print(f"PASS: local project queue, frozen source/workflow, macOS supervisor and confinement, passing/failing outcomes ({scratch})")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("telete", type=Path, help="absolute built Telete executable")
    parser.add_argument(
        "--scratch-parent", type=Path,
        default=Path(os.environ.get("TMPDIR", tempfile.gettempdir())),
        help="scratch parent (defaults to TMPDIR; resolves macOS temporary symlinks)",
    )
    parser.add_argument("--keep", action="store_true", help="retain a successful fixture too")
    args = parser.parse_args()
    require(sys.platform == "darwin", "this smoke test requires macOS")
    require(args.telete.is_absolute() and os.access(args.telete, os.X_OK), "select an absolute executable")
    parent = args.scratch_parent.resolve(strict=True)
    scratch = Path(tempfile.mkdtemp(prefix="telete-portability-smoke-", dir=parent))
    try:
        exercise(args.telete, scratch)
    except BaseException:
        print(f"Fixture retained for inspection: {scratch}", file=sys.stderr)
        raise
    else:
        if not args.keep:
            shutil.rmtree(scratch)


if __name__ == "__main__":
    main()
