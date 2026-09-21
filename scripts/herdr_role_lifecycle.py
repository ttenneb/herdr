#!/usr/bin/python3
"""External, one-shot lifecycle authority for durable Pi roles.

Pi extensions remain status-only. This manager starts solely from an explicit,
authority-bound activation and never treats Gate, transport, report, or Todo
receipts as model-turn triggers or acceptance evidence.
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import time
from typing import Any

ROLE_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$")
SHA256 = re.compile(r"^[0-9a-f]{64}$")
HUMAN_ROLES = {"user-facing-pm", "human-facing-controller"}
TERMINAL_STATES = {"completed", "failed", "hibernate_failed", "rejected"}
ACTIVATION_ID = re.compile(r"^(?:[0-9a-f]{32}|[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})$")
MAX_QUEUED_ITEMS = 32
MAX_QUEUED_PAYLOAD_BYTES = 65536
MAX_QUEUED_DEPTH = 8
MAX_CORRELATIONS = 32
ROLE_RATE_LIMIT = 8
ROLE_RATE_WINDOW_MS = 60_000
ROLE_QUEUE_LIMIT = 32


class LifecycleError(RuntimeError):
    pass


class LifecycleInhibited(LifecycleError):
    """A durable terminal/inhibit record makes automatic restart unsafe or needless."""


def below(path: Path, root: Path) -> bool:
    try:
        path.resolve(strict=False).relative_to(root.resolve(strict=False))
        return True
    except ValueError:
        return False


def validate_secure_parents(path: Path, durable_root: Path, label: str) -> None:
    root = durable_root.resolve(strict=True)
    if not path.is_absolute() or ".." in path.parts:
        raise LifecycleError(f"{label} must be an absolute normalized path below {durable_root}")
    try:
        path.relative_to(root)
    except ValueError as exc:
        raise LifecycleError(f"{label} must be an absolute path below {durable_root}") from exc
    current = path.parent
    while current != root:
        info = current.lstat()
        if stat.S_ISLNK(info.st_mode):
            raise LifecycleError(f"{label} parent {current} must not be a symlink")
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid():
            raise LifecycleError(f"{label} parent {current} must be an owner-controlled directory")
        if stat.S_IMODE(info.st_mode) & 0o022:
            raise LifecycleError(f"{label} parent {current} must not be group/world writable")
        if current.parent == current:
            raise LifecycleError(f"{label} parent escapes durable root")
        current = current.parent


def read_secure_bytes(path: Path, durable_root: Path, label: str, *, limit: int) -> bytes:
    validate_secure_parents(path, durable_root, label)
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as exc:
        raise LifecycleError(f"cannot securely open {label} {path}: {exc}") from exc
    try:
        info = os.fstat(descriptor)
        mode = stat.S_IMODE(info.st_mode)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid():
            raise LifecycleError(f"{label} must be a regular file owned by the effective user")
        if not mode & stat.S_IRUSR or mode & 0o077:
            raise LifecycleError(f"{label} mode must grant owner read and no group/world access")
        if info.st_size > limit:
            raise LifecycleError(f"{label} exceeds {limit} bytes")
        data = b""
        while len(data) <= limit:
            chunk = os.read(descriptor, min(65536, limit + 1 - len(data)))
            if not chunk:
                break
            data += chunk
        if len(data) > limit:
            raise LifecycleError(f"{label} exceeds {limit} bytes")
    finally:
        os.close(descriptor)
    validate_secure_parents(path, durable_root, label)
    return data


def decode_json(data: bytes, label: str) -> dict[str, Any]:
    try:
        value = json.loads(data.decode("utf-8"))
    except (UnicodeError, json.JSONDecodeError) as exc:
        raise LifecycleError(f"cannot decode {label} JSON: {exc}") from exc
    if not isinstance(value, dict):
        raise LifecycleError(f"{label} must contain a JSON object")
    return value


def read_json(path: Path, *, limit: int = 131072) -> dict[str, Any]:
    """Read manager-owned state, not authority material."""
    try:
        info = path.lstat()
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode) or info.st_size > limit:
            raise LifecycleError(f"invalid state file {path}")
        return decode_json(path.read_bytes(), str(path))
    except OSError as exc:
        raise LifecycleError(f"cannot read state file {path}: {exc}") from exc


def require_string(value: dict[str, Any], key: str, where: str) -> str:
    item = value.get(key)
    if not isinstance(item, str) or not item.strip():
        raise LifecycleError(f"{where}.{key} must be a nonempty string")
    return item


def require_digest(value: dict[str, Any], key: str, where: str) -> str:
    digest = require_string(value, key, where)
    if not SHA256.fullmatch(digest):
        raise LifecycleError(f"{where}.{key} must be a lowercase SHA-256 digest")
    return digest


def require_exact_session(value: Any, where: str) -> dict[str, str]:
    if not isinstance(value, dict):
        raise LifecycleError(f"{where} must be an object")
    result = {key: require_string(value, key, where) for key in ("agent", "kind", "source", "value")}
    if result["agent"] != "pi":
        raise LifecycleError(f"{where}.agent must be pi")
    return result


def exact_route(value: Any, where: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise LifecycleError(f"{where} must be an object")
    route = {key: require_string(value, key, where) for key in ("workspaceId", "paneId", "terminalId")}
    route["agentSession"] = require_exact_session(value.get("agentSession"), f"{where}.agentSession")
    if "name" in value:
        route["name"] = require_string(value, "name", where)
    return route


def role_route(role: dict[str, Any]) -> dict[str, Any]:
    return {"workspaceId": role["workspace"]["id"], "paneId": role["paneId"], "terminalId": role["terminalId"], "agentSession": role["agentSession"]}


def exact_lifecycle_recipient(role: dict[str, Any]) -> dict[str, Any]:
    return {"roleId": role["roleId"], "roleClass": role["roleClass"], "workspace": role["workspace"], "paneId": role["paneId"], "terminalId": role["terminalId"], "agentSession": role["agentSession"], "canonicalTask": role["task"], "mailboxPath": role["mailboxPath"], "reportRoute": role["reportRoute"]}


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
    task = {"id": require_string(task, "id", "manifest.task"), "source": require_string(task, "source", "manifest.task")}
    mailbox = Path(require_string(raw, "mailboxPath", "manifest"))
    state_dir = Path(require_string(raw, "stateDir", "manifest"))
    activation = Path(require_string(raw, "activationPath", "manifest"))
    for label, path in (("mailboxPath", mailbox), ("stateDir", state_dir), ("activationPath", activation)):
        if not path.is_absolute() or not below(path, durable_root):
            raise LifecycleError(f"manifest.{label} must be below durable root {durable_root}")
    report_route = exact_route(raw.get("reportRoute"), "manifest.reportRoute")
    issuer = exact_route(raw.get("authorizedIssuer"), "manifest.authorizedIssuer")
    executables = raw.get("executables")
    if not isinstance(executables, dict):
        raise LifecycleError("manifest.executables must be an object")
    bins = {key: executable(require_string(executables, key, "manifest.executables"), f"manifest.executables.{key}") for key in ("herdr", "pi", "python", "systemdNotify")}
    human_facing = raw.get("humanFacing", False)
    if not isinstance(human_facing, bool):
        raise LifecycleError("manifest.humanFacing must be boolean")
    grant_path = raw.get("humanFacingGrantPath")
    grant_digest = raw.get("humanFacingGrantDigest")
    if human_facing:
        if role_class not in HUMAN_ROLES:
            raise LifecycleError("humanFacing is allowed only for a user-facing PM or human-facing Controller")
        if not isinstance(grant_path, str):
            raise LifecycleError("humanFacing requires humanFacingGrantPath")
        require_digest(raw, "humanFacingGrantDigest", "manifest")
    elif grant_path is not None or grant_digest is not None:
        raise LifecycleError("human-facing grant material must be absent when humanFacing is false")
    result = dict(raw)
    result.update({"roleId": role_id, "roleClass": role_class, "workspace": {"id": workspace_id, "path": str(workspace_path)}, "paneId": pane_id, "terminalId": terminal_id, "agentSession": session, "task": task, "mailboxPath": str(mailbox), "stateDir": str(state_dir), "activationPath": str(activation), "reportRoute": report_route, "authorizedIssuer": issuer, "executables": bins, "humanFacing": human_facing})
    result["_manifestPath"] = str(manifest_path.resolve())
    if human_facing:
        grant_file = Path(str(grant_path))
        grant_bytes = read_secure_bytes(grant_file, durable_root, "human-facing grant", limit=32768)
        if hashlib.sha256(grant_bytes).hexdigest() != grant_digest:
            raise LifecycleError("human-facing grant digest mismatch")
        grant = decode_json(grant_bytes, "human-facing grant")
        expected = {"version": 1, "granted": True, "roleId": role_id, "roleClass": role_class, "canonicalTask": task, "issuer": issuer, "recipientRoute": role_route(result), "reportRoute": report_route}
        if grant != expected:
            raise LifecycleError("human-facing grant is not exactly bound to issuer/task/role/routes")
        result["humanFacingGrantPath"] = str(grant_file)
        result["humanFacingGrantDigest"] = grant_digest
    return result


def load_manifest(path: Path, durable_root: Path) -> dict[str, Any]:
    data = read_secure_bytes(path, durable_root, "manifest", limit=131072)
    role = validate_manifest(decode_json(data, "manifest"), path, durable_root)
    role["_manifestDigest"] = hashlib.sha256(data).hexdigest()
    return role


def atomic_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(value, stream, sort_keys=True, separators=(",", ":")); stream.write("\n"); stream.flush(); os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        try: os.unlink(temporary)
        except FileNotFoundError: pass


def fsync_directory(path: Path) -> None:
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def write_new_secure(path: Path, data: bytes) -> None:
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(descriptor, data); os.fsync(descriptor)
    finally:
        os.close(descriptor)
    fsync_directory(path.parent)


def receipt(role: dict[str, Any], phase: str, **extra: Any) -> dict[str, Any]:
    return {"version": 1, "atEpochMs": int(time.time() * 1000), "roleId": role["roleId"], "task": role["task"], "workspace": role["workspace"], "paneId": role["paneId"], "terminalId": role["terminalId"], "agentSession": role["agentSession"], "mailboxPath": role["mailboxPath"], "reportRoute": role["reportRoute"], "phase": phase, **extra}


def canonical_digest(value: Any) -> str:
    encoded = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def validate_queued_input_request(role: dict[str, Any], request: dict[str, Any]) -> dict[str, Any]:
    required = {"kind", "version", "activationId", "batchId", "itemIds", "priority", "correlation", "depth", "cause", "payload", "payloadSha256", "recipient"}
    if set(request) != required:
        raise LifecycleError("queued-input activation request fields do not match v1 exactly")
    if request.get("kind") != "pi-input-gate.queued-input-activation" or request.get("version") != 1:
        raise LifecycleError("queued-input activation kind/version mismatch")
    activation_id = require_string(request, "activationId", "request")
    if not ACTIVATION_ID.fullmatch(activation_id):
        raise LifecycleError("request.activationId must be a canonical random 128-bit ID")
    batch_id = require_string(request, "batchId", "request")
    if not ROLE_ID.fullmatch(batch_id):
        raise LifecycleError("request.batchId is out of bounds")
    item_ids = request.get("itemIds")
    if not isinstance(item_ids, list) or not 1 <= len(item_ids) <= MAX_QUEUED_ITEMS:
        raise LifecycleError("request.itemIds must contain 1..32 items")
    if any(not isinstance(item, str) or not ROLE_ID.fullmatch(item) for item in item_ids) or len(set(item_ids)) != len(item_ids):
        raise LifecycleError("request.itemIds must be unique bounded IDs")
    if request.get("priority") not in {"low", "normal", "high"}:
        raise LifecycleError("request.priority is invalid")
    depth = request.get("depth")
    if not isinstance(depth, int) or isinstance(depth, bool) or not 0 <= depth <= MAX_QUEUED_DEPTH:
        raise LifecycleError("request.depth exceeds the lifecycle guard")
    correlations = request.get("correlation")
    if not isinstance(correlations, list) or len(correlations) > MAX_CORRELATIONS:
        raise LifecycleError("request.correlation exceeds the lifecycle guard")
    seen = set()
    normalized_correlations = []
    for index, correlation in enumerate(correlations):
        if not isinstance(correlation, dict) or set(correlation) != {"namespace", "key", "revision"}:
            raise LifecycleError(f"request.correlation[{index}] is malformed")
        namespace = require_string(correlation, "namespace", f"request.correlation[{index}]")
        key = require_string(correlation, "key", f"request.correlation[{index}]")
        revision = correlation.get("revision")
        if len(namespace.encode()) > 128 or len(key.encode()) > 128 or not isinstance(revision, int) or isinstance(revision, bool) or not 0 <= revision <= 9_007_199_254_740_991:
            raise LifecycleError(f"request.correlation[{index}] is out of bounds")
        pair = (namespace, key)
        if pair in seen:
            raise LifecycleError("request.correlation repeats a namespace/key loop")
        seen.add(pair); normalized_correlations.append({"namespace": namespace, "key": key, "revision": revision})
    if request.get("cause") != "accepted_queued_input":
        raise LifecycleError("only accepted_queued_input can activate a managed role")
    payload = request.get("payload")
    if not isinstance(payload, str) or not payload or "\x00" in payload or len(payload.encode("utf-8")) > MAX_QUEUED_PAYLOAD_BYTES:
        raise LifecycleError("request.payload is empty, unsafe, or out of bounds")
    payload_digest = require_digest(request, "payloadSha256", "request")
    if hashlib.sha256(payload.encode("utf-8")).hexdigest() != payload_digest:
        raise LifecycleError("request payload digest mismatch")
    if request.get("recipient") != exact_lifecycle_recipient(role):
        raise LifecycleError("request recipient is not the exact managed lifecycle role")
    result = dict(request); result["correlation"] = normalized_correlations
    return result


def queue_record_paths(role: dict[str, Any], activation_id: str) -> tuple[Path, Path, Path]:
    root = Path(role["stateDir"]) / "queued-input-activations" / activation_id
    return root / "record.json", root / "activation.json", root / "payload.txt"


def update_queued_lifecycle_record(activation_path: Path | None, lifecycle_outcome: str) -> None:
    if activation_path is None or activation_path.name != "activation.json":
        return
    record_path = activation_path.with_name("record.json")
    queue_root = activation_path.parents[1]
    if not record_path.exists() or queue_root.name != "queued-input-activations":
        return
    lock_path = queue_root / "schedule.lock"
    lock = lock_path.open("a+")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        record = read_json(record_path)
        record["active"] = False
        record["lifecycleOutcome"] = lifecycle_outcome
        record["lifecycleUpdatedAtEpochMs"] = int(time.time() * 1000)
        atomic_json(record_path, record)
    finally:
        lock.close()


def queued_result(activation_id: str, outcome: str, *, receipt_id: str | None = None, reason: str | None = None) -> dict[str, Any]:
    result: dict[str, Any] = {"activationId": activation_id, "outcome": outcome}
    if receipt_id is not None: result["receiptId"] = receipt_id
    if reason is not None: result["reason"] = reason
    return result


def schedule_queued_input(role: dict[str, Any], request: dict[str, Any], durable_root: Path, invoking_issuer: dict[str, Any] | None, *, recover: bool = False, fault: str | None = None) -> dict[str, Any]:
    try:
        issuer = exact_route(invoking_issuer, "invoking issuer")
        if issuer != role["authorizedIssuer"]:
            raise LifecycleError("invoking tasking issuer is not authorized for this role")
        request = validate_queued_input_request(role, request)
    except LifecycleError as exc:
        raw_id = request.get("activationId") if isinstance(request, dict) else None
        return queued_result(raw_id if isinstance(raw_id, str) else "invalid", "rejected", reason=str(exc))
    activation_id = request["activationId"]
    record_path, activation_path, payload_path = queue_record_paths(role, activation_id)
    queue_root = record_path.parents[1]
    queue_root.mkdir(mode=0o700, parents=True, exist_ok=True); os.chmod(queue_root, 0o700)
    lock_path = queue_root / "schedule.lock"
    lock = lock_path.open("a+"); os.chmod(lock_path, 0o600)
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        request_digest = canonical_digest(request)
        identity = {"activationId": activation_id, "batchId": request["batchId"], "payloadSha256": request["payloadSha256"], "recipient": request["recipient"]}
        identity_digest = canonical_digest(identity)
        if record_path.exists():
            record = read_json(record_path)
            if record.get("identityDigest") != identity_digest or record.get("requestDigest") != request_digest:
                return queued_result(activation_id, "rejected", reason="activation ID conflicts with durable content")
            if record.get("outcome") == "scheduled":
                return queued_result(activation_id, "duplicate", receipt_id=record.get("receiptId"), reason="same activation was already scheduled")
            if record.get("outcome") == "uncertain":
                if not recover:
                    return queued_result(activation_id, "uncertain", receipt_id=record.get("receiptId"), reason="explicit same-ID recovery is required")
                try:
                    activation_bytes = read_secure_bytes(activation_path, durable_root, "queued-input activation", limit=131072)
                    payload_bytes = read_secure_bytes(payload_path, durable_root, "queued-input payload", limit=MAX_QUEUED_PAYLOAD_BYTES)
                except LifecycleError as exc:
                    return queued_result(activation_id, "uncertain", receipt_id=record.get("receiptId"), reason=f"recovery could not prove prior materialization: {exc}")
                if hashlib.sha256(activation_bytes).hexdigest() != record.get("activationArtifactSha256") or hashlib.sha256(payload_bytes).hexdigest() != request["payloadSha256"]:
                    return queued_result(activation_id, "uncertain", receipt_id=record.get("receiptId"), reason="recovery artifact digest mismatch")
                record["outcome"] = "scheduled"; record["recoveredAtEpochMs"] = int(time.time() * 1000); atomic_json(record_path, record)
                return queued_result(activation_id, "duplicate", receipt_id=record.get("receiptId"), reason="same-ID recovery proved the prior schedule; no second activation was created")
            return queued_result(activation_id, "rejected", reason="activation ID is terminal")
        if recover:
            return queued_result(activation_id, "rejected", reason="no uncertain activation exists for recovery")
        now = int(time.time() * 1000)
        active_records = []
        recent_correlations: dict[tuple[str, str], int] = {}
        recent_scheduled = 0
        for candidate in queue_root.glob("*/record.json"):
            try: previous = read_json(candidate)
            except LifecycleError: continue
            if previous.get("active") is True and previous.get("outcome") in {"scheduled", "uncertain"}: active_records.append(previous)
            scheduled_at = previous.get("scheduledAtEpochMs")
            if isinstance(scheduled_at, int) and now - scheduled_at < ROLE_RATE_WINDOW_MS:
                recent_scheduled += 1
                for correlation in previous.get("correlation", []):
                    if isinstance(correlation, dict):
                        pair = (correlation.get("namespace"), correlation.get("key")); revision = correlation.get("revision")
                        if all(isinstance(value, str) for value in pair) and isinstance(revision, int): recent_correlations[pair] = max(revision, recent_correlations.get(pair, -1))
        if len(active_records) >= ROLE_QUEUE_LIMIT:
            return queued_result(activation_id, "rejected", reason="per-role queued activation depth limit reached")
        if recent_scheduled >= ROLE_RATE_LIMIT:
            return queued_result(activation_id, "rejected", reason="per-role activation rate limit reached")
        for correlation in request["correlation"]:
            previous_revision = recent_correlations.get((correlation["namespace"], correlation["key"]))
            if previous_revision is not None and correlation["revision"] <= previous_revision:
                return queued_result(activation_id, "rejected", reason="correlation loop or stale revision rejected")
        record_path.parent.mkdir(mode=0o700, parents=True, exist_ok=False)
        fsync_directory(queue_root)
        receipt_id = hashlib.sha256(f"queued-input:{identity_digest}".encode()).hexdigest()
        record = {"version": 1, "activationId": activation_id, "identityDigest": identity_digest, "requestDigest": request_digest, "batchId": request["batchId"], "payloadSha256": request["payloadSha256"], "recipient": request["recipient"], "correlation": request["correlation"], "depth": request["depth"], "cause": "accepted_queued_input", "authorizedIssuer": role["authorizedIssuer"], "outcome": "uncertain", "active": True, "receiptId": receipt_id, "scheduledAtEpochMs": now}
        atomic_json(record_path, record)
        write_new_secure(payload_path, request["payload"].encode("utf-8"))
        activation = {"version": 1, "roleId": role["roleId"], "executionId": activation_id, "kind": "queued_input", "cause": "accepted_queued_input", "canonicalTask": role["task"], "issuer": role["authorizedIssuer"], "senderRoute": role["authorizedIssuer"], "parentRoute": role["reportRoute"], "promptPath": str(payload_path), "promptDigest": request["payloadSha256"], "queuedInputActivation": {"activationId": activation_id, "batchId": request["batchId"], "itemIds": request["itemIds"], "priority": request["priority"], "correlation": request["correlation"], "depth": request["depth"], "recipient": request["recipient"], "requestDigest": request_digest}}
        activation_bytes = (json.dumps(activation, sort_keys=True, separators=(",", ":")) + "\n").encode()
        write_new_secure(activation_path, activation_bytes)
        record["activationArtifactSha256"] = hashlib.sha256(activation_bytes).hexdigest(); atomic_json(record_path, record)
        if fault == "lost_ack":
            return queued_result(activation_id, "uncertain", receipt_id=receipt_id, reason="activation materialized but scheduling acknowledgement was lost")
        record["outcome"] = "scheduled"; atomic_json(record_path, record)
        return queued_result(activation_id, "scheduled", receipt_id=receipt_id)
    except OSError as exc:
        return queued_result(activation_id, "uncertain", reason=f"durable scheduling outcome is uncertain: {exc}")
    finally:
        lock.close()


def run_json(argv: list[str], *, timeout: float = 35, env: dict[str, str] | None = None) -> dict[str, Any]:
    try:
        process = subprocess.run(argv, check=False, capture_output=True, text=True, timeout=timeout, env=env)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise LifecycleError(f"command failed: {argv[0]}: {exc}") from exc
    if process.returncode != 0:
        raise LifecycleError(f"command exited {process.returncode}: {argv[0]}: {process.stderr.strip()}")
    try: value = json.loads(process.stdout)
    except json.JSONDecodeError as exc: raise LifecycleError(f"command returned malformed JSON: {argv[0]}") from exc
    if not isinstance(value, dict) or value.get("error") is not None:
        raise LifecycleError(f"command returned an error: {value}")
    return value


def agent_from(value: dict[str, Any]) -> dict[str, Any]:
    result = value.get("result")
    if not isinstance(result, dict) or not isinstance(result.get("agent"), dict):
        raise LifecycleError("Herdr response omitted agent")
    return result["agent"]


def assert_identity(role: dict[str, Any], agent: dict[str, Any], *, require_ready: bool, expected_name: str | None = None) -> None:
    expected = {"workspace_id": role["workspace"]["id"], "pane_id": role["paneId"], "terminal_id": role["terminalId"]}
    for key, wanted in expected.items():
        if agent.get(key) != wanted: raise LifecycleError(f"live {key} mismatch: expected {wanted!r}, got {agent.get(key)!r}")
    if Path(str(agent.get("cwd", ""))).resolve(strict=False) != Path(role["workspace"]["path"]).resolve(strict=False):
        raise LifecycleError("live workspace path mismatch")
    if expected_name is not None and agent.get("name") != expected_name:
        raise LifecycleError("live managed-agent generation does not match the activation")
    if require_ready and agent.get("interactive_ready") is not True: raise LifecycleError("Pi is not interactively ready")
    if require_ready and agent.get("agent_session") != role["agentSession"]: raise LifecycleError("live Pi session identity does not match the durable role")


def launch_args(role: dict[str, Any]) -> list[str]:
    argv = [role["executables"]["pi"], "--session", role["agentSession"]["value"]]
    if not role["humanFacing"]: argv += ["--exclude-tools", "ask_user_question"]
    return argv


def notify(role: dict[str, Any], *parts: str) -> None:
    process = subprocess.run([role["executables"]["systemdNotify"], *parts], check=False, capture_output=True, text=True)
    if process.returncode != 0: raise LifecycleError(f"systemd-notify failed: {process.stderr.strip()}")


def heartbeat(role: dict[str, Any], phase: str, state_dir: Path, detail: str = "") -> None:
    atomic_json(state_dir / "heartbeat.json", receipt(role, phase, detail=detail, managerPid=os.getpid()))
    notify(role, "WATCHDOG=1", f"STATUS={phase}: {detail}".rstrip())


def load_activation(role: dict[str, Any], durable_root: Path, activation_path: Path | None = None) -> dict[str, Any]:
    path = activation_path or Path(role["activationPath"])
    data = read_secure_bytes(path, durable_root, "activation", limit=131072)
    value = decode_json(data, "activation")
    if value.get("version") != 1 or value.get("roleId") != role["roleId"]: raise LifecycleError("activation version or roleId mismatch")
    execution_id = require_string(value, "executionId", "activation")
    if not ROLE_ID.fullmatch(execution_id): raise LifecycleError("activation.executionId has invalid characters")
    if value.get("kind") not in {"assignment", "report", "queued_input"}: raise LifecycleError("activation.kind is invalid")
    if value.get("kind") == "queued_input":
        if value.get("cause") != "accepted_queued_input": raise LifecycleError("queued activation cause must be accepted_queued_input")
        queued = value.get("queuedInputActivation")
        if not isinstance(queued, dict) or queued.get("activationId") != value.get("executionId") or queued.get("recipient") != exact_lifecycle_recipient(role): raise LifecycleError("queued activation identity/recipient mismatch")
        record_path = path.with_name("record.json")
        record = decode_json(read_secure_bytes(record_path, durable_root, "queued-input schedule record", limit=131072), "queued-input schedule record")
        if record.get("outcome") != "scheduled" or record.get("activationId") != queued.get("activationId") or record.get("requestDigest") != queued.get("requestDigest") or record.get("payloadSha256") != value.get("promptDigest") or record.get("recipient") != queued.get("recipient"):
            raise LifecycleError("queued activation lacks an exact scheduled record binding")
        if hashlib.sha256(data).hexdigest() != record.get("activationArtifactSha256"):
            raise LifecycleError("queued activation artifact digest mismatch")
    if value.get("canonicalTask") != role["task"]: raise LifecycleError("activation canonical task mismatch")
    if value.get("issuer") != role["authorizedIssuer"] or value.get("senderRoute") != role["authorizedIssuer"]: raise LifecycleError("activation issuer/sender route mismatch")
    if value.get("parentRoute") != role["reportRoute"]: raise LifecycleError("activation parent route mismatch")
    prompt_path = Path(require_string(value, "promptPath", "activation"))
    prompt_digest = require_digest(value, "promptDigest", "activation")
    prompt_bytes = read_secure_bytes(prompt_path, durable_root, "activation prompt", limit=65536)
    if hashlib.sha256(prompt_bytes).hexdigest() != prompt_digest: raise LifecycleError("activation prompt digest mismatch")
    try: prompt = prompt_bytes.decode("utf-8")
    except UnicodeError as exc: raise LifecycleError("activation prompt must be UTF-8") from exc
    if not prompt.strip() or "\x00" in prompt: raise LifecycleError("activation prompt must be nonempty text without NUL")
    result = dict(value); result["prompt"] = prompt; result["activationDigest"] = hashlib.sha256(data).hexdigest()
    return result


def exact_hibernate(role: dict[str, Any], generation: str, *, timeout: float = 8) -> None:
    herdr = role["executables"]["herdr"]
    run_json([herdr, "agent", "send-keys", role["paneId"], "--expected-terminal", role["terminalId"], "--expected-name", generation, "--", "ctrl+d"], timeout=5)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        time.sleep(0.05)
        current = agent_from(run_json([herdr, "agent", "get", role["paneId"]], timeout=5))
        if current.get("terminal_id") != role["terminalId"]: raise LifecycleError("terminal changed while confirming hibernate")
        if current.get("agent_status") in {"unknown", "exited"} and current.get("agent") is None: return
    raise LifecycleError("exact managed-agent generation stayed live after graceful hibernate")


def rollback_started(role: dict[str, Any], activation: dict[str, Any], state_dir: Path, generation: str, cause: Exception) -> str:
    disposition = "uncertain"
    detail = str(cause)
    try:
        exact_hibernate(role, generation)
        disposition = "completed"
    except Exception as cleanup_error:
        detail = f"{detail}; cleanup={cleanup_error}"
        try:
            current = agent_from(run_json([role["executables"]["herdr"], "agent", "get", role["paneId"]], timeout=5))
            if current.get("terminal_id") == role["terminalId"] and current.get("name") == generation:
                disposition = "failed"
        except Exception:
            disposition = "uncertain"
    rollback = receipt(role, "rollback", executionId=activation["executionId"], activationDigest=activation["activationDigest"], promptDigest=activation["promptDigest"], generation=generation, preStartIdentity=activation.get("preStartIdentity"), disposition=disposition, detail=detail)
    atomic_json(state_dir / "rollback-receipt.json", rollback)
    if disposition != "completed":
        atomic_json(state_dir / "relaunch-inhibit.json", receipt(role, "relaunch_inhibited", executionId=activation["executionId"], generation=generation, rollbackDisposition=disposition, detail=detail))
    return disposition


def lifecycle_run(role: dict[str, Any], durable_root: Path, poll_seconds: float, idle_timeout: float, activation_path: Path | None = None) -> None:
    state_dir = Path(role["stateDir"]); state_dir.mkdir(mode=0o700, parents=True, exist_ok=True); os.chmod(state_dir, 0o700)
    lock_stream = (state_dir / "manager.lock").open("a+")
    try: fcntl.flock(lock_stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError as exc: raise LifecycleError("another lifecycle manager owns this role") from exc
    try:
        if (state_dir / "relaunch-inhibit.json").exists():
            update_queued_lifecycle_record(activation_path, "relaunch_inhibited")
            raise LifecycleInhibited("automatic relaunch is inhibited pending explicit recovery")
        activation = load_activation(role, durable_root, activation_path)
        current_path = state_dir / "activation-receipt.json"
        if current_path.exists():
            previous = read_json(current_path)
            if previous.get("executionId") == activation["executionId"] and previous.get("phase") in TERMINAL_STATES: raise LifecycleInhibited("activation executionId is already terminal and cannot be replayed")
        herdr = role["executables"]["herdr"]
        initial = agent_from(run_json([herdr, "agent", "get", role["paneId"]])); assert_identity(role, initial, require_ready=False)
        if initial.get("agent_status") not in {"unknown", "exited", None} or initial.get("agent") is not None:
            atomic_json(state_dir / "relaunch-inhibit.json", receipt(role, "relaunch_inhibited", executionId=activation["executionId"], detail="pane was not provably hibernated before start"))
            update_queued_lifecycle_record(activation_path, "ambiguous_live_process_inhibited")
            raise LifecycleInhibited("pane was not provably hibernated; refusing ambiguous attach/retry")
        generation = f"{role['roleId']}-{activation['executionId']}-{activation['promptDigest'][:12]}"
        activation["preStartIdentity"] = {key: initial.get(key) for key in ("workspace_id", "pane_id", "terminal_id", "agent", "agent_status", "agent_session", "name", "revision", "state_change_seq")}
        atomic_json(current_path, receipt(role, "validated", executionId=activation["executionId"], activationDigest=activation["activationDigest"], promptDigest=activation["promptDigest"], issuer=activation["issuer"], senderRoute=activation["senderRoute"], parentRoute=activation["parentRoute"], generation=generation, preStartIdentity=activation["preStartIdentity"], gateAdmission="unknown", modelExecution="unknown", todoAcceptance="unknown"))
        argv = [herdr, "agent", "start", generation, "--kind", "pi", "--pane", role["paneId"], "--timeout", "30000", "--", *launch_args(role)[1:]]
        launch_env = dict(os.environ); launch_env["PATH"] = str(Path(role["executables"]["pi"]).parent) + os.pathsep + launch_env.get("PATH", "")
        try:
            started = run_json(argv, timeout=40, env=launch_env)
            live = agent_from(started); assert_identity(role, live, require_ready=True, expected_name=generation)
            atomic_json(current_path, receipt(role, "started", executionId=activation["executionId"], activationDigest=activation["activationDigest"], promptDigest=activation["promptDigest"], generation=generation, createdIdentity={key: live.get(key) for key in ("workspace_id", "pane_id", "terminal_id", "agent_session", "name", "revision", "state_change_seq")}, gateAdmission="unknown", modelExecution="unknown", todoAcceptance="unknown"))
            baseline_seq = int(live.get("state_change_seq", 0))
            transported = agent_from(run_json([herdr, "agent", "prompt", role["paneId"], activation["prompt"]])); assert_identity(role, transported, require_ready=False, expected_name=generation)
        except Exception as failure:
            disposition = rollback_started(role, activation, state_dir, generation, failure)
            atomic_json(current_path, receipt(role, "failed", executionId=activation["executionId"], activationDigest=activation["activationDigest"], promptDigest=activation["promptDigest"], generation=generation, rollbackDisposition=disposition, error=str(failure), gateAdmission="unknown", modelExecution="unknown", todoAcceptance="unknown"))
            update_queued_lifecycle_record(activation_path, f"failed_rollback_{disposition}")
            raise LifecycleError(f"post-start failure; rollback {disposition}: {failure}") from failure
        atomic_json(current_path, receipt(role, "transport_accepted", executionId=activation["executionId"], activationDigest=activation["activationDigest"], promptDigest=activation["promptDigest"], generation=generation, runtimeTransportAccepted=True, gateAdmission="unknown", modelExecution="unknown", reportAcceptance="unknown", todoAcceptance="unknown"))
        notify(role, "READY=1", "WATCHDOG=1", "STATUS=Pi interactive; activation transport accepted"); heartbeat(role, "executing", state_dir, f"execution={activation['executionId']}")
        try:
            deadline = time.monotonic() + idle_timeout; observed_activity = False
            while time.monotonic() < deadline:
                time.sleep(poll_seconds)
                live = agent_from(run_json([herdr, "agent", "get", role["paneId"]])); assert_identity(role, live, require_ready=False, expected_name=generation)
                sequence = int(live.get("state_change_seq", 0)); status = live.get("agent_status")
                if status in {"working", "blocked"} or sequence > baseline_seq: observed_activity = True
                heartbeat(role, "executing", state_dir, f"status={status} sequence={sequence}")
                if observed_activity and status in {"idle", "done"}: break
            else: raise LifecycleError("Pi did not return to idle before the execution timeout")
            exact_hibernate(role, generation, timeout=15)
        except Exception as failure:
            disposition = rollback_started(role, activation, state_dir, generation, failure)
            atomic_json(current_path, receipt(role, "failed", executionId=activation["executionId"], generation=generation, rollbackDisposition=disposition, error=str(failure), gateAdmission="unknown", modelExecution="unknown", todoAcceptance="unknown"))
            update_queued_lifecycle_record(activation_path, f"failed_rollback_{disposition}")
            raise LifecycleError(f"execution/hibernate failure; rollback {disposition}: {failure}") from failure
        atomic_json(current_path, receipt(role, "completed", executionId=activation["executionId"], activationDigest=activation["activationDigest"], promptDigest=activation["promptDigest"], generation=generation, runtimeTransportAccepted=True, gateAdmission="unknown", modelExecution="unknown", reportAcceptance="unknown", todoAcceptance="unknown", hibernated=True)); heartbeat(role, "hibernated", state_dir, f"execution={activation['executionId']}")
        update_queued_lifecycle_record(activation_path, "completed")
    finally: lock_stream.close()


def render_unit(role: dict[str, Any], manager: Path) -> str:
    if not manager.is_absolute() or not manager.is_file(): raise LifecycleError("--manager must be an absolute regular file")
    return f"""[Unit]\nDescription=Herdr one-shot Pi role {role['roleId']}\nAfter=herdr.service\nStartLimitIntervalSec=300\nStartLimitBurst=3\n\n[Service]\nType=notify\nNotifyAccess=main\nExecStart={role['executables']['python']} {manager} run --manifest {role['_manifestPath']}\nRestart=on-failure\nRestartSec=15s\nWatchdogSec=120s\nTimeoutStartSec=60s\nTimeoutStopSec=30s\nKillMode=process\n\n[Install]\nWantedBy=default.target\n"""


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(); sub = result.add_subparsers(dest="command", required=True)
    for name in ("validate", "launch-argv", "run", "render-unit", "schedule-queued-input", "recover-queued-input"):
        item = sub.add_parser(name); item.add_argument("--manifest", required=True); item.add_argument("--durable-root", default="/home")
        if name == "run":
            item.add_argument("--poll-seconds", type=float, default=2.0); item.add_argument("--execution-timeout", type=float, default=14400.0); item.add_argument("--activation-id")
        if name == "render-unit": item.add_argument("--manager", required=True)
        if name in {"schedule-queued-input", "recover-queued-input"}:
            item.add_argument("--request", required=True); item.add_argument("--issuer", required=True)
    return result


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        role = load_manifest(Path(args.manifest), Path(args.durable_root))
        if args.command == "validate": print(json.dumps({"valid": True, "roleId": role["roleId"], "humanFacing": role["humanFacing"]}, sort_keys=True))
        elif args.command == "launch-argv": print(json.dumps({"argv": launch_args(role), "humanFacingGranted": role["humanFacing"]}, sort_keys=True))
        elif args.command == "render-unit": print(render_unit(role, Path(args.manager)), end="")
        elif args.command in {"schedule-queued-input", "recover-queued-input"}:
            request_data = read_secure_bytes(Path(args.request), Path(args.durable_root), "queued-input request", limit=131072)
            request = decode_json(request_data, "queued-input request")
            issuer_data = read_secure_bytes(Path(args.issuer), Path(args.durable_root), "tasking issuer route", limit=32768)
            issuer = decode_json(issuer_data, "tasking issuer route")
            print(json.dumps(schedule_queued_input(role, request, Path(args.durable_root), issuer, recover=args.command == "recover-queued-input"), sort_keys=True))
        else:
            activation_path = None
            if args.activation_id:
                if not ACTIVATION_ID.fullmatch(args.activation_id): raise LifecycleError("--activation-id is invalid")
                activation_path = queue_record_paths(role, args.activation_id)[1]
            lifecycle_run(role, Path(args.durable_root), args.poll_seconds, args.execution_timeout, activation_path)
        return 0
    except LifecycleInhibited as exc:
        print(f"herdr-role-lifecycle: {exc}", file=sys.stderr); return 0
    except (LifecycleError, OSError, UnicodeError) as exc:
        print(f"herdr-role-lifecycle: {exc}", file=sys.stderr); return 1


if __name__ == "__main__": raise SystemExit(main())
