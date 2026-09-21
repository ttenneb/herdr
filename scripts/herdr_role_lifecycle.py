#!/usr/bin/python3
"""External, one-shot lifecycle authority for durable Pi roles.

This process is deliberately outside Pi and its extensions. It starts only from an
explicit activation artifact and emits transport/activation receipts, never Gate,
model-execution, report-acceptance, or Todo-acceptance claims.
"""
from __future__ import annotations

import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import tempfile
import time
from typing import Any

ROLE_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$")
HUMAN_ROLES = {"user-facing-pm", "human-facing-controller"}
TERMINAL_STATES = {"completed", "failed", "hibernate_failed", "rejected"}


class LifecycleError(RuntimeError):
    pass


def read_json(path: Path, *, limit: int = 131072) -> dict[str, Any]:
    try:
        info = path.lstat()
    except OSError as exc:
        raise LifecycleError(f"cannot inspect {path}: {exc}") from exc
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise LifecycleError(f"{path} must be a regular non-link file")
    if info.st_size > limit:
        raise LifecycleError(f"{path} exceeds {limit} bytes")
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise LifecycleError(f"cannot read JSON from {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise LifecycleError(f"{path} must contain a JSON object")
    return value


def require_string(value: dict[str, Any], key: str, where: str) -> str:
    item = value.get(key)
    if not isinstance(item, str) or not item.strip():
        raise LifecycleError(f"{where}.{key} must be a nonempty string")
    return item


def require_exact_session(value: Any, where: str) -> dict[str, str]:
    if not isinstance(value, dict):
        raise LifecycleError(f"{where} must be an object")
    result = {key: require_string(value, key, where) for key in ("agent", "kind", "source", "value")}
    if result["agent"] != "pi":
        raise LifecycleError(f"{where}.agent must be pi")
    return result


def below(path: Path, root: Path) -> bool:
    try:
        path.resolve(strict=False).relative_to(root.resolve(strict=False))
        return True
    except ValueError:
        return False


def executable(path: str, field: str) -> str:
    candidate = Path(path)
    if not candidate.is_absolute() or not candidate.is_file() or not os.access(candidate, os.X_OK):
        raise LifecycleError(f"{field} must be an absolute executable file")
    return str(candidate)


def validate_manifest(raw: dict[str, Any], manifest_path: Path, durable_root: Path) -> dict[str, Any]:
    if raw.get("version") != 1:
        raise LifecycleError("manifest.version must be 1")
    role_id = require_string(raw, "roleId", "manifest")
    if not ROLE_ID.fullmatch(role_id):
        raise LifecycleError("manifest.roleId has invalid characters")
    role_class = require_string(raw, "roleClass", "manifest")
    workspace = raw.get("workspace")
    if not isinstance(workspace, dict):
        raise LifecycleError("manifest.workspace must be an object")
    workspace_id = require_string(workspace, "id", "manifest.workspace")
    workspace_path = Path(require_string(workspace, "path", "manifest.workspace"))
    if not workspace_path.is_absolute():
        raise LifecycleError("manifest.workspace.path must be absolute")
    pane_id = require_string(raw, "paneId", "manifest")
    terminal_id = require_string(raw, "terminalId", "manifest")
    session = require_exact_session(raw.get("agentSession"), "manifest.agentSession")
    task = raw.get("task")
    if not isinstance(task, dict):
        raise LifecycleError("manifest.task must be an object")
    require_string(task, "id", "manifest.task")
    require_string(task, "source", "manifest.task")
    mailbox = Path(require_string(raw, "mailboxPath", "manifest"))
    state_dir = Path(require_string(raw, "stateDir", "manifest"))
    activation = Path(require_string(raw, "activationPath", "manifest"))
    for label, path in (("mailboxPath", mailbox), ("stateDir", state_dir), ("activationPath", activation)):
        if not path.is_absolute() or not below(path, durable_root):
            raise LifecycleError(f"manifest.{label} must be below durable root {durable_root}")
    route = raw.get("reportRoute")
    if not isinstance(route, dict):
        raise LifecycleError("manifest.reportRoute must be an object")
    for key in ("workspaceId", "paneId", "terminalId"):
        require_string(route, key, "manifest.reportRoute")
    require_exact_session(route.get("agentSession"), "manifest.reportRoute.agentSession")
    executables = raw.get("executables")
    if not isinstance(executables, dict):
        raise LifecycleError("manifest.executables must be an object")
    bins = {key: executable(require_string(executables, key, "manifest.executables"), f"manifest.executables.{key}") for key in ("herdr", "pi", "python", "systemdNotify")}
    human_facing = raw.get("humanFacing", False)
    if not isinstance(human_facing, bool):
        raise LifecycleError("manifest.humanFacing must be boolean")
    grant = raw.get("humanFacingGrant")
    if human_facing:
        if role_class not in HUMAN_ROLES:
            raise LifecycleError("humanFacing is allowed only for a user-facing PM or human-facing Controller")
        if not isinstance(grant, dict) or grant.get("granted") is not True:
            raise LifecycleError("humanFacing requires an explicit granted humanFacingGrant")
        require_string(grant, "grantedBy", "manifest.humanFacingGrant")
        require_string(grant, "authorityTask", "manifest.humanFacingGrant")
    elif grant is not None:
        raise LifecycleError("humanFacingGrant must be absent when humanFacing is false")
    result = dict(raw)
    result.update({"roleId": role_id, "roleClass": role_class, "workspace": {"id": workspace_id, "path": str(workspace_path)}, "paneId": pane_id, "terminalId": terminal_id, "agentSession": session, "mailboxPath": str(mailbox), "stateDir": str(state_dir), "activationPath": str(activation), "executables": bins, "humanFacing": human_facing})
    result["_manifestPath"] = str(manifest_path.resolve())
    return result


def atomic_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(value, stream, sort_keys=True, separators=(",", ":"))
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def receipt(role: dict[str, Any], phase: str, **extra: Any) -> dict[str, Any]:
    return {"version": 1, "atEpochMs": int(time.time() * 1000), "roleId": role["roleId"], "task": role["task"], "workspace": role["workspace"], "paneId": role["paneId"], "terminalId": role["terminalId"], "agentSession": role["agentSession"], "mailboxPath": role["mailboxPath"], "reportRoute": role["reportRoute"], "phase": phase, **extra}


def run_json(argv: list[str], *, timeout: float = 35, env: dict[str, str] | None = None) -> dict[str, Any]:
    try:
        process = subprocess.run(argv, check=False, capture_output=True, text=True, timeout=timeout, env=env)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise LifecycleError(f"command failed: {argv[0]}: {exc}") from exc
    if process.returncode != 0:
        raise LifecycleError(f"command exited {process.returncode}: {argv[0]}: {process.stderr.strip()}")
    try:
        value = json.loads(process.stdout)
    except json.JSONDecodeError as exc:
        raise LifecycleError(f"command returned malformed JSON: {argv[0]}") from exc
    if not isinstance(value, dict) or value.get("error") is not None:
        raise LifecycleError(f"command returned an error: {value}")
    return value


def agent_from(value: dict[str, Any]) -> dict[str, Any]:
    result = value.get("result")
    if not isinstance(result, dict):
        raise LifecycleError("Herdr response omitted result")
    agent = result.get("agent")
    if not isinstance(agent, dict):
        raise LifecycleError("Herdr response omitted agent")
    return agent


def assert_identity(role: dict[str, Any], agent: dict[str, Any], *, require_ready: bool) -> None:
    expected = {"workspace_id": role["workspace"]["id"], "pane_id": role["paneId"], "terminal_id": role["terminalId"]}
    for key, wanted in expected.items():
        if agent.get(key) != wanted:
            raise LifecycleError(f"live {key} mismatch: expected {wanted!r}, got {agent.get(key)!r}")
    if Path(str(agent.get("cwd", ""))).resolve(strict=False) != Path(role["workspace"]["path"]).resolve(strict=False):
        raise LifecycleError("live workspace path mismatch")
    if require_ready and agent.get("interactive_ready") is not True:
        raise LifecycleError("Pi is not interactively ready")
    if require_ready and agent.get("agent_session") != role["agentSession"]:
        raise LifecycleError("live Pi session identity does not match the durable role")


def launch_args(role: dict[str, Any]) -> list[str]:
    session = role["agentSession"]
    argv = [role["executables"]["pi"], "--session", session["value"]]
    if not role["humanFacing"]:
        argv += ["--exclude-tools", "ask_user_question"]
    return argv


def notify(role: dict[str, Any], *parts: str) -> None:
    process = subprocess.run([role["executables"]["systemdNotify"], *parts], check=False, capture_output=True, text=True)
    if process.returncode != 0:
        raise LifecycleError(f"systemd-notify failed: {process.stderr.strip()}")


def heartbeat(role: dict[str, Any], phase: str, state_dir: Path, detail: str = "") -> None:
    atomic_json(state_dir / "heartbeat.json", receipt(role, phase, detail=detail, managerPid=os.getpid()))
    notify(role, "WATCHDOG=1", f"STATUS={phase}: {detail}".rstrip())


def load_activation(role: dict[str, Any], durable_root: Path) -> dict[str, Any]:
    value = read_json(Path(role["activationPath"]), limit=65536)
    if value.get("version") != 1 or value.get("roleId") != role["roleId"]:
        raise LifecycleError("activation version or roleId mismatch")
    execution_id = require_string(value, "executionId", "activation")
    if not ROLE_ID.fullmatch(execution_id):
        raise LifecycleError("activation.executionId has invalid characters")
    if value.get("kind") not in {"assignment", "report"}:
        raise LifecycleError("activation.kind must be assignment or report")
    prompt_path = Path(require_string(value, "promptPath", "activation"))
    if not prompt_path.is_absolute() or not below(prompt_path, durable_root):
        raise LifecycleError("activation.promptPath must be below the durable root")
    prompt_info = prompt_path.lstat()
    if stat.S_ISLNK(prompt_info.st_mode) or not stat.S_ISREG(prompt_info.st_mode) or prompt_info.st_size > 65536:
        raise LifecycleError("activation prompt must be a regular non-link file no larger than 65536 bytes")
    prompt = prompt_path.read_text(encoding="utf-8")
    if not prompt.strip() or "\x00" in prompt:
        raise LifecycleError("activation prompt must be nonempty text without NUL")
    value = dict(value)
    value["prompt"] = prompt
    return value


def lifecycle_run(role: dict[str, Any], durable_root: Path, poll_seconds: float, idle_timeout: float) -> None:
    state_dir = Path(role["stateDir"])
    state_dir.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(state_dir, 0o700)
    lock_stream = (state_dir / "manager.lock").open("a+")
    try:
        fcntl.flock(lock_stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as exc:
        raise LifecycleError("another lifecycle manager owns this role") from exc
    try:
        activation = load_activation(role, durable_root)
        current_path = state_dir / "activation-receipt.json"
        if current_path.exists():
            previous = read_json(current_path)
            if previous.get("executionId") == activation["executionId"] and previous.get("phase") in TERMINAL_STATES:
                raise LifecycleError("activation executionId is already terminal and cannot be replayed")
        atomic_json(current_path, receipt(role, "validated", executionId=activation["executionId"], activationKind=activation["kind"], gateAdmission="unknown", modelExecution="unknown", todoAcceptance="unknown"))
        herdr = role["executables"]["herdr"]
        initial = agent_from(run_json([herdr, "agent", "get", role["paneId"]]))
        assert_identity(role, initial, require_ready=False)
        status = initial.get("agent_status")
        already_running = initial.get("agent") == "pi" and status not in {"unknown", "exited", None}
        if already_running:
            assert_identity(role, initial, require_ready=True)
            live = initial
        else:
            if status not in {"unknown", "exited", None}:
                raise LifecycleError(f"pane is occupied by a non-hibernated process with status {status!r}")
            argv = [herdr, "agent", "start", role["roleId"], "--kind", "pi", "--pane", role["paneId"], "--timeout", "30000", "--", *launch_args(role)[1:]]
            launch_env = dict(os.environ)
            pi_dir = str(Path(role["executables"]["pi"]).parent)
            launch_env["PATH"] = pi_dir + os.pathsep + launch_env.get("PATH", "")
            started = run_json(argv, timeout=40, env=launch_env)
            live = agent_from(started)
            assert_identity(role, live, require_ready=True)
        baseline_seq = int(live.get("state_change_seq", 0))
        prompted = run_json([herdr, "agent", "prompt", role["paneId"], activation["prompt"]])
        transported = agent_from(prompted)
        assert_identity(role, transported, require_ready=False)
        atomic_json(current_path, receipt(role, "transport_accepted", executionId=activation["executionId"], activationKind=activation["kind"], runtimeTransportAccepted=True, gateAdmission="unknown", modelExecution="unknown", reportAcceptance="unknown", todoAcceptance="unknown"))
        notify(role, "READY=1", "WATCHDOG=1", "STATUS=Pi interactive; activation transport accepted")
        heartbeat(role, "executing", state_dir, f"execution={activation['executionId']}")
        deadline = time.monotonic() + idle_timeout
        observed_activity = False
        while time.monotonic() < deadline:
            time.sleep(poll_seconds)
            live = agent_from(run_json([herdr, "agent", "get", role["paneId"]]))
            assert_identity(role, live, require_ready=False)
            sequence = int(live.get("state_change_seq", 0))
            status = live.get("agent_status")
            if status in {"working", "blocked"} or sequence > baseline_seq:
                observed_activity = True
            heartbeat(role, "executing", state_dir, f"status={status} sequence={sequence}")
            if observed_activity and status in {"idle", "done"}:
                break
        else:
            raise LifecycleError("Pi did not return to idle before the execution timeout")
        run_json([herdr, "agent", "send-keys", role["paneId"], "ctrl+d"])
        hibernate_deadline = time.monotonic() + 15
        while time.monotonic() < hibernate_deadline:
            time.sleep(poll_seconds)
            live = agent_from(run_json([herdr, "agent", "get", role["paneId"]]))
            assert_identity(role, live, require_ready=False)
            if live.get("agent_status") in {"unknown", "exited"}:
                atomic_json(current_path, receipt(role, "completed", executionId=activation["executionId"], activationKind=activation["kind"], runtimeTransportAccepted=True, gateAdmission="unknown", modelExecution="unknown", reportAcceptance="unknown", todoAcceptance="unknown", hibernated=True))
                heartbeat(role, "hibernated", state_dir, f"execution={activation['executionId']}")
                return
        atomic_json(current_path, receipt(role, "hibernate_failed", executionId=activation["executionId"], runtimeTransportAccepted=True, gateAdmission="unknown", todoAcceptance="unknown", hibernated=False))
        raise LifecycleError("Pi stayed live after graceful hibernate request; no force-kill attempted")
    finally:
        lock_stream.close()


def render_unit(role: dict[str, Any], manager: Path) -> str:
    python = role["executables"]["python"]
    if not manager.is_absolute() or not manager.is_file():
        raise LifecycleError("--manager must be an absolute regular file")
    manifest = role["_manifestPath"]
    return f"""[Unit]\nDescription=Herdr one-shot Pi role {role['roleId']}\nAfter=herdr.service\nStartLimitIntervalSec=300\nStartLimitBurst=3\n\n[Service]\nType=notify\nNotifyAccess=main\nExecStart={python} {manager} run --manifest {manifest}\nRestart=on-failure\nRestartSec=15s\nWatchdogSec=120s\nTimeoutStartSec=60s\nTimeoutStopSec=30s\nKillMode=process\n\n[Install]\nWantedBy=default.target\n"""


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser()
    sub = result.add_subparsers(dest="command", required=True)
    for name in ("validate", "launch-argv", "run", "render-unit"):
        item = sub.add_parser(name)
        item.add_argument("--manifest", required=True)
        item.add_argument("--durable-root", default="/home")
        if name == "run":
            item.add_argument("--poll-seconds", type=float, default=2.0)
            item.add_argument("--execution-timeout", type=float, default=14400.0)
        if name == "render-unit":
            item.add_argument("--manager", required=True)
    return result


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        manifest_path = Path(args.manifest)
        durable_root = Path(args.durable_root)
        role = validate_manifest(read_json(manifest_path), manifest_path, durable_root)
        if args.command == "validate":
            print(json.dumps({"valid": True, "roleId": role["roleId"], "humanFacing": role["humanFacing"]}, sort_keys=True))
        elif args.command == "launch-argv":
            print(json.dumps({"argv": launch_args(role), "humanFacingGranted": role["humanFacing"]}, sort_keys=True))
        elif args.command == "render-unit":
            print(render_unit(role, Path(args.manager)), end="")
        else:
            lifecycle_run(role, durable_root, args.poll_seconds, args.execution_timeout)
        return 0
    except (LifecycleError, OSError, UnicodeError) as exc:
        print(f"herdr-role-lifecycle: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
