import fcntl
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import tempfile
import unittest

MODULE_PATH = Path(__file__).with_name("herdr_mailbox_transport.py")
spec = importlib.util.spec_from_file_location("herdr_mailbox_transport", MODULE_PATH)
transport = importlib.util.module_from_spec(spec)
assert spec.loader
spec.loader.exec_module(transport)


class MailboxTransportDecisionTests(unittest.TestCase):
    def setUp(self):
        scratch = Path.home() / ".local/share/herdr/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix="herdr-mailbox-transport-", dir=scratch))
        self.addCleanup(lambda: shutil.rmtree(self.root, ignore_errors=True))
        self.state = self.root / "state"
        self.state.mkdir(mode=0o700)
        self.mailbox = self.root / "mailbox.jsonl"
        self.mailbox.touch(mode=0o600)
        self.runtime = self.root / "runtime.json"
        self.manager_lock = self.state / "manager.lock"
        self.systemctl = self.root / "systemctl"
        self.systemctl.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        self.systemctl.chmod(0o700)
        self.recipient = {
            "workspaceId": "w1",
            "paneId": "w1:p1",
            "terminalId": "term1",
            "agentSession": {
                "agent": "pi",
                "kind": "path",
                "source": "herdr:pi",
                "value": str(self.root / "session.jsonl"),
            },
        }
        self.manifest = {
            "version": 1,
            "recipient": self.recipient,
            "mailboxPath": str(self.mailbox),
            "runtimePath": str(self.runtime),
            "stateDir": str(self.state),
            "managerLockPath": str(self.manager_lock),
            "wakeUnit": "herdr-pi-recipient@example.service",
            "systemctlPath": str(self.systemctl),
        }
        self.request = {
            "kind": "herdr.mailbox.delivery",
            "version": 1,
            "deliveryId": "0123456789abcdef0123456789abcdef",
            "recipient": self.recipient,
            "mailboxPath": str(self.mailbox),
        }

    def secure_json(self, path, value):
        path.write_text(json.dumps(value, sort_keys=True), encoding="utf-8")
        path.chmod(0o600)

    def runtime_value(self, state, generation=1, endpoint=None):
        value = {
            "version": 1,
            "state": state,
            "generation": generation,
            "recipient": self.recipient,
            "mailboxPath": str(self.mailbox),
            "protocol": transport.MAILBOX_PROTOCOL,
            "build": {
                "version": "0.8.4-stabilize.test",
                "channel": "stabilize",
                "buildId": "test",
                "sourceCommit": "a" * 40,
            },
        }
        if endpoint is not None:
            value["endpointPath"] = str(endpoint)
        return value

    def test_compatible_live_pi_receives_directly_and_surfaces_build_identity(self):
        endpoint = self.root / "live.sock"
        self.secure_json(self.runtime, self.runtime_value("live", endpoint=endpoint))
        sent = []
        def exchange(path, frame, _root):
            sent.append((path, frame))
            return {
                "kind": "herdr.mailbox.delivery-result",
                "version": 1,
                "deliveryId": self.request["deliveryId"],
                "outcome": "accepted",
                "receiptId": "live-receipt",
            }
        starts = []
        result = transport.dispatch(self.manifest, self.request, self.root, exchange=exchange,
                                    runner=lambda *a, **k: starts.append((a, k)))
        self.assertEqual(result["route"], "live_direct")
        self.assertEqual(result["outcome"], "accepted")
        self.assertEqual(result["build"]["sourceCommit"], "a" * 40)
        self.assertEqual(sent[0][0], endpoint)
        self.assertEqual(sent[0][1]["mailboxPath"], str(self.mailbox))
        self.assertEqual(starts, [])

    def test_sleeping_or_stopped_recipient_uses_one_nonblocking_systemd_wake(self):
        for state in ("sleeping", "stopped"):
            with self.subTest(state=state):
                shutil.rmtree(self.state)
                self.state.mkdir(mode=0o700)
                self.secure_json(self.runtime, self.runtime_value(state))
                calls = []
                def runner(command, **kwargs):
                    calls.append((command, kwargs))
                    return __import__("subprocess").CompletedProcess(command, 0, "", "")
                first = transport.dispatch(self.manifest, self.request, self.root, runner=runner)
                second = transport.dispatch(self.manifest, self.request, self.root, runner=runner)
                self.assertEqual(first["route"], "systemd_wake")
                self.assertEqual(first["outcome"], "accepted")
                self.assertEqual(second["outcome"], "duplicate")
                self.assertEqual(len(calls), 1)
                self.assertEqual(calls[0][0], [str(self.systemctl), "--user", "start", "--no-block", self.manifest["wakeUnit"]])

    def test_two_deliveries_share_one_generation_scoped_wake_receipt(self):
        self.secure_json(self.runtime, self.runtime_value("sleeping", generation=9))
        calls = []
        def runner(command, **kwargs):
            calls.append(command)
            return __import__("subprocess").CompletedProcess(command, 0, "", "")
        first = transport.dispatch(self.manifest, self.request, self.root, runner=runner)
        second_request = dict(self.request)
        second_request["deliveryId"] = "1123456789abcdef0123456789abcdef"
        second = transport.dispatch(self.manifest, second_request, self.root, runner=runner)
        self.assertEqual(first["outcome"], "accepted")
        self.assertEqual(second["outcome"], "duplicate")
        self.assertEqual(first["receiptId"], second["receiptId"])
        self.assertEqual(first["receiptId"], transport.wake_receipt_id(self.manifest, 9))
        self.assertNotIn(self.request["deliveryId"], first["receiptId"])
        self.assertNotIn(second_request["deliveryId"], second["receiptId"])
        self.assertEqual(len(calls), 1)

    def test_mailbox_path_is_stable_across_sleep_wake_and_live_registration(self):
        self.secure_json(self.runtime, self.runtime_value("sleeping", generation=7))
        calls = []
        def runner(command, **kwargs):
            calls.append(command)
            return __import__("subprocess").CompletedProcess(command, 0, "", "")
        wake = transport.dispatch(self.manifest, self.request, self.root, runner=runner)
        self.assertEqual(wake["mailboxPath"], str(self.mailbox))
        endpoint = self.root / "woken.sock"
        self.secure_json(self.runtime, self.runtime_value("live", generation=8, endpoint=endpoint))
        frames = []
        def exchange(_path, frame, _root):
            frames.append(frame)
            return {"kind": "herdr.mailbox.delivery-result", "version": 1,
                    "deliveryId": frame["deliveryId"], "outcome": "duplicate",
                    "receiptId": "mailbox-existing"}
        direct = transport.dispatch(self.manifest, self.request, self.root, exchange=exchange, runner=runner)
        self.assertEqual(direct["mailboxPath"], wake["mailboxPath"])
        self.assertEqual(frames[0]["mailboxPath"], wake["mailboxPath"])
        self.assertEqual(len(calls), 1)

    def test_held_role_manager_lock_rejects_lock_cycle_without_starting(self):
        self.secure_json(self.runtime, self.runtime_value("sleeping"))
        lock = self.manager_lock.open("a+")
        self.addCleanup(lock.close)
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        calls = []
        result = transport.dispatch(self.manifest, self.request, self.root,
                                    runner=lambda *args, **kwargs: calls.append((args, kwargs)))
        self.assertEqual(result["outcome"], "rejected")
        self.assertEqual(result["reason"], "lifecycle_manager_lock_held")
        self.assertEqual(calls, [])

    def test_incompatible_live_registration_does_not_fall_back_to_wake(self):
        runtime = self.runtime_value("live", endpoint=self.root / "live.sock")
        runtime["protocol"] = "other/v9"
        self.secure_json(self.runtime, runtime)
        calls = []
        result = transport.dispatch(self.manifest, self.request, self.root,
                                    runner=lambda *args, **kwargs: calls.append((args, kwargs)))
        self.assertEqual(result["outcome"], "rejected")
        self.assertEqual(result["reason"], "incompatible_live_recipient")
        self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()
