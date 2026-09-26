#!/usr/bin/python3
"""Direct transport for a durable Pi mailbox.

The mailbox owner publishes one runtime registration. A compatible live process
receives a bounded request on its Unix socket. A sleeping or stopped
registration is rejected with `wake_disabled`: Herdr wakes panes it put to
sleep in-process, and this module never starts anything. It never uses a PTY.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import socket
import stat
import subprocess
from typing import Any

MAILBOX_PROTOCOL = "herdr.pi-mailbox/v1"
WAKE_DISABLED_REASON = "wake_disabled: sleeping recipients are woken in-process by Herdr, not by mailbox dispatch"
DELIVERY_ID = re.compile(r"^[0-9a-f]{32}$")
UNIT_NAME = re.compile(r"^[A-Za-z0-9_.@:-]{1,255}\.service$")
MAX_JSON_BYTES = 64 * 1024
MAX_SOCKET_PATH_BYTES = 100


class TransportError(RuntimeError):
    pass


def _below(path: Path, root: Path) -> bool:
    try:
        path.resolve(strict=False).relative_to(root.resolve(strict=False))
        return True
    except ValueError:
        return False


def _secure_parents(path: Path, root: Path, label: str) -> None:
    root = root.resolve(strict=True)
    if not path.is_absolute() or ".." in path.parts or not _below(path, root):
        raise TransportError(f"{label} must be an absolute normalized path below {root}")
    current = path.parent
    while current != root:
        info = current.lstat()
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
            raise TransportError(f"{label} parent is not a real directory")
        if info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) & 0o022:
            raise TransportError(f"{label} parent is not owner controlled")
        current = current.parent


def _read_secure_json(path: Path, root: Path, label: str) -> dict[str, Any]:
    _secure_parents(path, root, label)
    flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        descriptor = os.open(path, flags)
    except OSError as exc:
        raise TransportError(f"cannot open {label}: {exc}") from exc
    try:
        info = os.fstat(descriptor)
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid()
                or stat.S_IMODE(info.st_mode) & 0o077 or info.st_size > MAX_JSON_BYTES):
            raise TransportError(f"{label} must be a bounded owner-only regular file")
        data = os.read(descriptor, MAX_JSON_BYTES + 1)
    finally:
        os.close(descriptor)
    if len(data) > MAX_JSON_BYTES:
        raise TransportError(f"{label} exceeds {MAX_JSON_BYTES} bytes")
    try:
        value = json.loads(data)
    except (UnicodeError, json.JSONDecodeError) as exc:
        raise TransportError(f"cannot decode {label}: {exc}") from exc
    if not isinstance(value, dict):
        raise TransportError(f"{label} must contain an object")
    return value


def _exact_recipient(value: Any, label: str) -> dict[str, Any]:
    fields = {"workspaceId", "paneId", "terminalId", "agentSession"}
    if not isinstance(value, dict) or set(value) != fields:
        raise TransportError(f"{label} must be an exact recipient identity")
    session = value.get("agentSession")
    if not isinstance(session, dict) or set(session) != {"agent", "kind", "source", "value"}:
        raise TransportError(f"{label}.agentSession is invalid")
    strings = [value[key] for key in ("workspaceId", "paneId", "terminalId")] + list(session.values())
    if session.get("agent") != "pi" or any(not isinstance(item, str) or not item or len(item.encode()) > 4096 or any(ord(c) < 0x20 for c in item) for item in strings):
        raise TransportError(f"{label} is not a bounded Pi recipient")
    return value


def _validate_manifest(manifest: dict[str, Any], root: Path) -> dict[str, Any]:
    required = {"version", "recipient", "mailboxPath", "runtimePath", "stateDir", "managerLockPath", "wakeUnit", "systemctlPath"}
    if set(manifest) != required or manifest.get("version") != 1:
        raise TransportError("transport manifest fields/version are invalid")
    result = dict(manifest)
    result["recipient"] = _exact_recipient(manifest.get("recipient"), "manifest.recipient")
    for field in ("mailboxPath", "runtimePath", "stateDir", "managerLockPath"):
        raw = manifest.get(field)
        if not isinstance(raw, str):
            raise TransportError(f"manifest.{field} must be a path")
        path = Path(raw)
        if not path.is_absolute() or not _below(path, root):
            raise TransportError(f"manifest.{field} must be below the durable root")
        result[field] = str(path)
    mailbox = Path(result["mailboxPath"])
    _secure_parents(mailbox, root, "mailbox")
    info = mailbox.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) & 0o077:
        raise TransportError("mailbox must be an owner-only regular file")
    unit = manifest.get("wakeUnit")
    if not isinstance(unit, str) or not UNIT_NAME.fullmatch(unit):
        raise TransportError("manifest.wakeUnit is invalid")
    systemctl = Path(str(manifest.get("systemctlPath", "")))
    if not systemctl.is_absolute() or not systemctl.is_file() or not os.access(systemctl, os.X_OK):
        raise TransportError("manifest.systemctlPath must be an absolute executable")
    return result


def _validate_request(request: dict[str, Any], manifest: dict[str, Any]) -> dict[str, Any]:
    if set(request) != {"kind", "version", "deliveryId", "recipient", "mailboxPath"}:
        raise TransportError("delivery request fields are invalid")
    if request.get("kind") != "herdr.mailbox.delivery" or request.get("version") != 1:
        raise TransportError("delivery request kind/version is invalid")
    if not isinstance(request.get("deliveryId"), str) or not DELIVERY_ID.fullmatch(request["deliveryId"]):
        raise TransportError("deliveryId must be a random 128-bit lowercase hex ID")
    if request.get("recipient") != manifest["recipient"]:
        raise TransportError("delivery recipient does not match the manifest")
    if request.get("mailboxPath") != manifest["mailboxPath"]:
        raise TransportError("delivery mailbox path does not match the stable manifest path")
    return request


def _validate_build(value: Any) -> dict[str, Any]:
    required = {"version", "channel", "buildId", "sourceCommit"}
    if not isinstance(value, dict) or set(value) != required:
        raise TransportError("runtime build identity is invalid")
    for key in required:
        item = value[key]
        if item is not None and (not isinstance(item, str) or not item.strip() or len(item.encode()) > 256):
            raise TransportError("runtime build identity is invalid")
    if not isinstance(value["version"], str) or not isinstance(value["channel"], str):
        raise TransportError("runtime build identity omits version/channel")
    return value


def _validate_runtime(value: dict[str, Any], manifest: dict[str, Any]) -> dict[str, Any]:
    required = {"version", "state", "generation", "recipient", "mailboxPath", "protocol", "build"}
    allowed = required | {"endpointPath"}
    if not required.issubset(value) or not set(value).issubset(allowed) or value.get("version") != 1:
        raise TransportError("runtime registration fields/version are invalid")
    if value.get("state") not in {"live", "sleeping", "stopped"}:
        raise TransportError("runtime state is invalid")
    if not isinstance(value.get("generation"), int) or isinstance(value["generation"], bool) or value["generation"] < 1:
        raise TransportError("runtime generation is invalid")
    if value.get("recipient") != manifest["recipient"] or value.get("mailboxPath") != manifest["mailboxPath"]:
        raise TransportError("runtime registration changed recipient or mailbox path")
    value = dict(value)
    value["build"] = _validate_build(value.get("build"))
    if value["state"] == "live":
        endpoint = value.get("endpointPath")
        if not isinstance(endpoint, str) or not Path(endpoint).is_absolute() or len(os.fsencode(endpoint)) > MAX_SOCKET_PATH_BYTES:
            raise TransportError("live runtime endpoint is invalid")
    elif "endpointPath" in value:
        raise TransportError("sleeping/stopped runtime must not advertise a live endpoint")
    return value


def exchange_socket(path: Path, frame: dict[str, Any], root: Path, *, timeout: float = 5) -> dict[str, Any]:
    _secure_parents(path, root, "live endpoint")
    info = path.lstat()
    if not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o600:
        raise TransportError("live endpoint must be an owner-only Unix socket")
    payload = json.dumps(frame, sort_keys=True, separators=(",", ":")).encode() + b"\n"
    if len(payload) > MAX_JSON_BYTES:
        raise TransportError("delivery frame is too large")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(timeout)
        connection.connect(str(path))
        if not hasattr(socket, "SO_PEERCRED"):
            raise TransportError("platform cannot authenticate the live endpoint")
        peer = connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12)
        if int.from_bytes(peer[4:8], byteorder=os.sys.byteorder, signed=True) != os.geteuid():
            raise TransportError("live endpoint peer owner mismatch")
        connection.sendall(payload)
        connection.shutdown(socket.SHUT_WR)
        response = b""
        while len(response) <= MAX_JSON_BYTES:
            chunk = connection.recv(min(65536, MAX_JSON_BYTES + 1 - len(response)))
            if not chunk:
                break
            response += chunk
    if len(response) > MAX_JSON_BYTES or not response.endswith(b"\n"):
        raise TransportError("live endpoint response is missing or oversized")
    try:
        value = json.loads(response[:-1])
    except (UnicodeError, json.JSONDecodeError) as exc:
        raise TransportError(f"live endpoint response is malformed: {exc}") from exc
    return value


def _result(request: dict[str, Any], outcome: str, route: str, mailbox: str, *, build: dict[str, Any] | None = None, receipt_id: str | None = None, reason: str | None = None) -> dict[str, Any]:
    result = {"kind": "herdr.mailbox.dispatch-result", "version": 1, "deliveryId": request.get("deliveryId", "invalid"), "outcome": outcome, "route": route, "mailboxPath": mailbox}
    if build is not None:
        result["build"] = build
    if receipt_id is not None:
        result["receiptId"] = receipt_id
    if reason is not None:
        result["reason"] = reason
    return result


def _validate_live_result(request: dict[str, Any], value: Any) -> dict[str, Any]:
    allowed = {"kind", "version", "deliveryId", "outcome", "receiptId", "reason"}
    if not isinstance(value, dict) or not {"kind", "version", "deliveryId", "outcome"}.issubset(value) or not set(value).issubset(allowed):
        raise TransportError("live delivery result is malformed")
    if value.get("kind") != "herdr.mailbox.delivery-result" or value.get("version") != 1 or value.get("deliveryId") != request["deliveryId"]:
        raise TransportError("live delivery result identity mismatch")
    if value.get("outcome") not in {"accepted", "duplicate", "uncertain", "rejected"}:
        raise TransportError("live delivery outcome is invalid")
    if value["outcome"] in {"accepted", "duplicate"} and not isinstance(value.get("receiptId"), str):
        raise TransportError("live delivery result omits its receipt")
    return value


def dispatch(manifest: dict[str, Any], request: dict[str, Any], durable_root: Path, *, exchange: Any = exchange_socket, runner: Any = subprocess.run) -> dict[str, Any]:
    """Select exactly one route without waiting for a newly started role manager."""
    root = durable_root.resolve(strict=True)
    try:
        manifest = _validate_manifest(manifest, root)
        request = _validate_request(request, manifest)
        runtime = _validate_runtime(_read_secure_json(Path(manifest["runtimePath"]), root, "runtime registration"), manifest)
    except TransportError as exc:
        return _result(request, "rejected", "none", str(manifest.get("mailboxPath", "")), reason=str(exc))

    if runtime["state"] == "live":
        if runtime["protocol"] != MAILBOX_PROTOCOL:
            return _result(request, "rejected", "none", manifest["mailboxPath"], build=runtime["build"], reason="incompatible_live_recipient")
        try:
            live = _validate_live_result(request, exchange(Path(runtime["endpointPath"]), request, root))
        except (TransportError, OSError, TimeoutError, socket.timeout) as exc:
            return _result(request, "uncertain", "live_direct", manifest["mailboxPath"], build=runtime["build"], reason=str(exc))
        return _result(request, live["outcome"], "live_direct", manifest["mailboxPath"], build=runtime["build"], receipt_id=live.get("receiptId"), reason=live.get("reason"))

    # The systemd wake that used to follow replayed a static role activation and
    # could never deliver the message. Herdr now wakes panes it put to sleep
    # in-process (`herdr agent sleep`, App::wake_pane), and lifecycle roles are
    # activated only through `start-queued-input`. Nothing is started here.
    return _result(request, "rejected", "none", manifest["mailboxPath"], build=runtime["build"], reason=WAKE_DISABLED_REASON)

def _load_plain_json(path: Path, root: Path, label: str) -> dict[str, Any]:
    return _read_secure_json(path, root, label)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Route a durable Pi mailbox delivery directly or schedule one wake")
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--request", required=True)
    parser.add_argument("--durable-root", default="/home")
    args = parser.parse_args(argv)
    root = Path(args.durable_root)
    try:
        manifest = _load_plain_json(Path(args.manifest), root, "transport manifest")
        request = _load_plain_json(Path(args.request), root, "delivery request")
        result = dispatch(manifest, request, root)
    except TransportError as exc:
        result = {"kind": "herdr.mailbox.dispatch-result", "version": 1, "deliveryId": "invalid", "outcome": "rejected", "route": "none", "mailboxPath": "", "reason": str(exc)}
    print(json.dumps(result, sort_keys=True))
    return 0 if result["outcome"] in {"accepted", "duplicate"} else 1


if __name__ == "__main__":
    raise SystemExit(main())
