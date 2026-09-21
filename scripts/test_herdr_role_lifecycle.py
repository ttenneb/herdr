import importlib.util
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import unittest

MODULE_PATH = Path(__file__).with_name("herdr_role_lifecycle.py")
spec = importlib.util.spec_from_file_location("herdr_role_lifecycle", MODULE_PATH)
lifecycle = importlib.util.module_from_spec(spec)
assert spec.loader
spec.loader.exec_module(lifecycle)


class RoleLifecycleTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="herdr-role-lifecycle-", dir=Path.home() / ".local/share/herdr/scratch"))
        self.addCleanup(lambda: shutil.rmtree(self.root, ignore_errors=True))
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.events = self.root / "events"
        self.pi = self.executable("pi", "#!/bin/sh\nexit 0\n")
        self.python = Path(shutil.which("python3")).resolve()
        self.notify = self.executable("systemd-notify", f"#!/bin/sh\nprintf 'notify %s\\n' \"$*\" >> {self.events}\n")
        self.herdr = self.executable("herdr", self.fake_herdr())
        self.prompt = self.root / "prompt.txt"
        self.prompt.write_text("Do the exact bounded assignment.\n", encoding="utf-8")
        self.activation = self.root / "role/activation.json"
        self.activation.parent.mkdir()
        self.activation.write_text(json.dumps({"version": 1, "roleId": "owner-1", "executionId": "exec-1", "kind": "assignment", "promptPath": str(self.prompt)}), encoding="utf-8")
        self.manifest_path = self.root / "role/role.json"
        self.manifest = self.base_manifest()
        self.write_manifest()

    def executable(self, name, content):
        path = self.bin / name
        path.write_text(content, encoding="utf-8")
        path.chmod(0o700)
        return path

    def fake_herdr(self):
        state = self.root / "get-count"
        session = {"agent": "pi", "kind": "path", "source": "herdr:pi", "value": str(self.root / "session.jsonl")}
        common = {"workspace_id": "w1", "pane_id": "w1:p2", "terminal_id": "term2", "cwd": str(self.root / "worktree"), "agent": "pi", "agent_session": session, "interactive_ready": True}
        return f'''#!/usr/bin/python3
import json, pathlib, sys
state=pathlib.Path({str(state)!r}); events=pathlib.Path({str(self.events)!r})
def emit(x): print(json.dumps({{"id":"fake","result":x}}))
def agent(status, seq, ready=True):
 x={common!r}; x.update(agent_status=status,state_change_seq=seq,interactive_ready=ready); return x
args=sys.argv[1:]; events.parent.mkdir(parents=True,exist_ok=True)
with events.open("a") as f: f.write("herdr "+" ".join(args[:2])+"\\n")
if args[:2]==["agent","get"]:
 n=int(state.read_text()) if state.exists() else 0; state.write_text(str(n+1))
 if n==0: emit({{"agent":agent("unknown",0,False)}})
 elif n==1: emit({{"agent":agent("working",2)}})
 elif n==2: emit({{"agent":agent("idle",3)}})
 else: emit({{"agent":agent("unknown",4,False)}})
elif args[:2]==["agent","start"]: emit({{"agent":agent("idle",1)}})
elif args[:2]==["agent","prompt"]:
 assert args[2]=="w1:p2" and args[3]=="Do the exact bounded assignment.\\n"; emit({{"agent":agent("idle",1)}})
elif args[:2]==["agent","send-keys"]:
 assert args[2:]==["w1:p2","ctrl+d"]; emit({{"agent":agent("idle",3)}})
else: sys.exit(9)
'''

    def base_manifest(self):
        worktree = self.root / "worktree"
        worktree.mkdir(exist_ok=True)
        session = {"agent": "pi", "kind": "path", "source": "herdr:pi", "value": str(self.root / "session.jsonl")}
        return {"version": 1, "roleId": "owner-1", "roleClass": "implementation-owner", "workspace": {"id": "w1", "path": str(worktree)}, "paneId": "w1:p2", "terminalId": "term2", "agentSession": session, "task": {"id": "100", "source": "todo"}, "mailboxPath": str(self.root / "mailbox.jsonl"), "reportRoute": {"workspaceId": "w0", "paneId": "w0:p1", "terminalId": "term1", "agentSession": session}, "stateDir": str(self.root / "role/state"), "activationPath": str(self.activation), "executables": {"herdr": str(self.herdr), "pi": str(self.pi), "python": str(self.python), "systemdNotify": str(self.notify)}, "humanFacing": False}

    def write_manifest(self):
        self.manifest_path.write_text(json.dumps(self.manifest), encoding="utf-8")

    def validated(self):
        return lifecycle.validate_manifest(lifecycle.read_json(self.manifest_path), self.manifest_path, self.root)

    def test_default_deny_and_explicit_narrow_human_facing_grant(self):
        role = self.validated()
        self.assertEqual(lifecycle.launch_args(role)[-2:], ["--exclude-tools", "ask_user_question"])
        self.manifest["humanFacing"] = True
        self.manifest["humanFacingGrant"] = {"granted": True, "grantedBy": "controller", "authorityTask": "100"}
        self.write_manifest()
        with self.assertRaisesRegex(lifecycle.LifecycleError, "allowed only"):
            self.validated()
        self.manifest["roleClass"] = "user-facing-pm"
        self.write_manifest()
        self.assertNotIn("ask_user_question", lifecycle.launch_args(self.validated()))

    def test_manifest_fails_closed_on_relative_executable_and_inexact_route(self):
        self.manifest["executables"]["herdr"] = "herdr"
        self.write_manifest()
        with self.assertRaisesRegex(lifecycle.LifecycleError, "absolute executable"):
            self.validated()
        self.manifest = self.base_manifest()
        del self.manifest["reportRoute"]["agentSession"]
        self.write_manifest()
        with self.assertRaisesRegex(lifecycle.LifecycleError, "agentSession"):
            self.validated()

    def test_unit_has_truthful_notify_and_bounded_restart_policy(self):
        unit = lifecycle.render_unit(self.validated(), MODULE_PATH.resolve())
        self.assertIn("Type=notify", unit)
        self.assertIn("NotifyAccess=main", unit)
        self.assertIn("Restart=on-failure", unit)
        self.assertIn("RestartSec=15s", unit)
        self.assertIn("StartLimitBurst=3", unit)
        self.assertIn("WatchdogSec=120s", unit)
        self.assertIn(f"ExecStart={self.python} {MODULE_PATH.resolve()} run --manifest {self.manifest_path.resolve()}", unit)
        self.assertNotIn("ExecStart=herdr", unit)
        self.assertNotIn("READY=1", unit)

    def test_run_starts_one_exact_process_transports_then_hibernates(self):
        role = self.validated()
        lifecycle.lifecycle_run(role, self.root, 0.001, 1)
        final = json.loads((Path(role["stateDir"]) / "activation-receipt.json").read_text())
        self.assertEqual(final["phase"], "completed")
        self.assertTrue(final["hibernated"])
        self.assertEqual(final["gateAdmission"], "unknown")
        self.assertEqual(final["todoAcceptance"], "unknown")
        events = self.events.read_text().splitlines()
        self.assertEqual(sum(line == "herdr agent start" for line in events), 1)
        self.assertLess(events.index("herdr agent prompt"), next(i for i, line in enumerate(events) if line.startswith("notify READY=1")))
        self.assertIn("herdr agent send-keys", events)
        heartbeat = json.loads((Path(role["stateDir"]) / "heartbeat.json").read_text())
        self.assertEqual(heartbeat["phase"], "hibernated")

    def test_terminal_activation_is_not_replayed(self):
        role = self.validated()
        lifecycle.lifecycle_run(role, self.root, 0.001, 1)
        with self.assertRaisesRegex(lifecycle.LifecycleError, "already terminal"):
            lifecycle.lifecycle_run(role, self.root, 0.001, 1)

    def test_no_receipt_or_mailbox_watcher_can_self_activate(self):
        source = MODULE_PATH.read_text(encoding="utf-8")
        self.assertIn('value.get("kind") not in {"assignment", "report"}', source)
        self.assertNotIn("inotify", source)
        self.assertNotIn("gate_inbox", source)
        self.assertNotIn("agent prompt\", role[\"mailboxPath", source)


if __name__ == "__main__":
    unittest.main()
