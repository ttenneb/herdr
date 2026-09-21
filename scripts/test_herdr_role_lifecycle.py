import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest import mock

MODULE_PATH = Path(__file__).with_name("herdr_role_lifecycle.py")
spec = importlib.util.spec_from_file_location("herdr_role_lifecycle", MODULE_PATH)
lifecycle = importlib.util.module_from_spec(spec)
assert spec.loader
spec.loader.exec_module(lifecycle)


class RoleLifecycleTests(unittest.TestCase):
    def setUp(self):
        scratch = Path.home() / ".local/share/herdr/scratch"
        scratch.mkdir(parents=True, exist_ok=True)
        self.root = Path(tempfile.mkdtemp(prefix="herdr-role-lifecycle-", dir=scratch))
        self.addCleanup(lambda: shutil.rmtree(self.root, ignore_errors=True))
        self.bin = self.root / "bin"; self.bin.mkdir(mode=0o700)
        self.role_dir = self.root / "role"; self.role_dir.mkdir(mode=0o700)
        self.worktree = self.root / "worktree"; self.worktree.mkdir(mode=0o700)
        self.events = self.root / "events"
        self.mode = self.root / "mode"; self.secure_write(self.mode, "normal")
        self.pi = self.executable("pi", "#!/bin/sh\nexit 0\n")
        self.python = Path(shutil.which("python3")).resolve()
        self.notify = self.executable("systemd-notify", f"#!/bin/sh\nprintf 'notify %s\\n' \"$*\" >> {self.events}\n")
        self.herdr = self.executable("herdr", self.fake_herdr())
        self.session = {"agent": "pi", "kind": "path", "source": "herdr:pi", "value": str(self.root / "session.jsonl")}
        self.issuer = self.route("issuer", "wi", "wi:p1", "termi")
        self.parent = self.route("parent", "wp", "wp:p1", "termp")
        self.prompt = self.root / "prompt.txt"; self.secure_write(self.prompt, "Do the exact bounded assignment.\n")
        self.activation = self.role_dir / "activation.json"
        self.manifest_path = self.role_dir / "role.json"
        self.manifest = self.base_manifest()
        self.write_activation(); self.write_manifest()

    def secure_write(self, path, content):
        path.write_text(content, encoding="utf-8"); path.chmod(0o600)

    def executable(self, name, content):
        path = self.bin / name; path.write_text(content, encoding="utf-8"); path.chmod(0o700); return path

    def route(self, name, workspace, pane, terminal):
        return {"name": name, "workspaceId": workspace, "paneId": pane, "terminalId": terminal, "agentSession": self.session if hasattr(self, "session") else {"agent": "pi", "kind": "path", "source": "herdr:pi", "value": str(self.root / "session.jsonl")}}

    def base_manifest(self):
        return {"version": 1, "roleId": "owner-1", "roleClass": "implementation-owner", "workspace": {"id": "w1", "path": str(self.worktree)}, "paneId": "w1:p2", "terminalId": "term2", "agentSession": self.session, "task": {"id": "100", "source": "todo"}, "mailboxPath": str(self.root / "mailbox.jsonl"), "reportRoute": self.parent, "authorizedIssuer": self.issuer, "stateDir": str(self.role_dir / "state"), "activationPath": str(self.activation), "executables": {"herdr": str(self.herdr), "pi": str(self.pi), "python": str(self.python), "systemdNotify": str(self.notify)}, "humanFacing": False}

    def activation_value(self):
        prompt_digest = hashlib.sha256(self.prompt.read_bytes()).hexdigest()
        return {"version": 1, "roleId": "owner-1", "executionId": "exec-1", "kind": "assignment", "canonicalTask": self.manifest["task"], "issuer": self.issuer, "senderRoute": self.issuer, "parentRoute": self.parent, "promptPath": str(self.prompt), "promptDigest": prompt_digest}

    def write_activation(self, value=None): self.secure_write(self.activation, json.dumps(value or self.activation_value(), sort_keys=True))
    def write_manifest(self): self.secure_write(self.manifest_path, json.dumps(self.manifest, sort_keys=True))
    def validated(self): return lifecycle.load_manifest(self.manifest_path, self.root)

    def fake_herdr(self):
        state = self.root / "fake-state.json"
        return f'''#!/usr/bin/python3
import json,pathlib,sys
state=pathlib.Path({str(state)!r}); mode=pathlib.Path({str(self.mode)!r}).read_text().strip(); events=pathlib.Path({str(self.events)!r})
session={{"agent":"pi","kind":"path","source":"herdr:pi","value":{str(self.root / "session.jsonl")!r}}}
def load(): return json.loads(state.read_text()) if state.exists() else {{"phase":"empty","name":None,"gets":0}}
def save(x): state.write_text(json.dumps(x))
def emit(x): print(json.dumps({{"id":"fake","result":x}}))
def agent(s,status,ready=True): return {{"workspace_id":"w1","pane_id":"w1:p2","terminal_id":"term2","cwd":{str(self.worktree)!r},"agent":"pi" if s["phase"]!="empty" else None,"agent_session":session if s["phase"]!="empty" else None,"interactive_ready":ready,"name":s.get("name"),"agent_status":status,"state_change_seq":s.get("gets",0)+1,"revision":9}}
a=sys.argv[1:]; events.parent.mkdir(parents=True,exist_ok=True)
with events.open("a") as f: f.write("herdr "+" ".join(a[:2])+"\\n")
s=load()
if a[:2]==["agent","get"]:
 if mode=="cleanup_uncertain" and s["phase"]=="started" and s.get("cleanup_failed"): print("bad"); sys.exit(0)
 if s["phase"]=="empty": emit({{"agent":agent(s,"unknown",False)}})
 elif s["phase"]=="exited": emit({{"agent":agent({{"phase":"empty","name":None,"gets":s["gets"]}},"unknown",False)}})
 else:
  s["gets"]+=1; save(s); emit({{"agent":agent(s,"working" if s["gets"]==1 else "idle")}})
elif a[:2]==["agent","start"]:
 s={{"phase":"started","name":a[2],"gets":0}}; save(s)
 if mode in ("start_malformed","cleanup_uncertain"): print("bad")
 else: emit({{"agent":agent(s,"idle")}})
elif a[:2]==["agent","prompt"]:
 if mode=="transport_malformed": print("bad")
 else: s["phase"]="prompted"; save(s); emit({{"agent":agent(s,"idle")}})
elif a[:2]==["agent","send-keys"]:
 assert "--expected-terminal" in a and "--expected-name" in a and a[-1]=="ctrl+d"
 assert a[a.index("--expected-terminal")+1]=="term2" and a[a.index("--expected-name")+1]==s["name"]
 if mode in ("cleanup_failed","cleanup_uncertain"):
  s["cleanup_failed"]=True; save(s); print("cleanup failed",file=sys.stderr); sys.exit(1)
 s["phase"]="exited"; save(s); emit({{"type":"ok"}})
else: sys.exit(9)
'''

    def test_default_deny_and_exact_human_facing_grant(self):
        self.assertEqual(lifecycle.launch_args(self.validated())[-2:], ["--exclude-tools", "ask_user_question"])
        self.manifest["humanFacing"] = True; self.manifest["roleClass"] = "user-facing-pm"
        grant = {"version": 1, "granted": True, "roleId": "owner-1", "roleClass": "user-facing-pm", "canonicalTask": self.manifest["task"], "issuer": self.issuer, "recipientRoute": {"workspaceId": "w1", "paneId": "w1:p2", "terminalId": "term2", "agentSession": self.session}, "reportRoute": self.parent}
        grant_path = self.role_dir / "grant.json"; self.secure_write(grant_path, json.dumps(grant, sort_keys=True))
        self.manifest["humanFacingGrantPath"] = str(grant_path); self.manifest["humanFacingGrantDigest"] = hashlib.sha256(grant_path.read_bytes()).hexdigest(); self.write_manifest()
        self.assertNotIn("ask_user_question", lifecycle.launch_args(self.validated()))
        grant["canonicalTask"] = {"id": "other", "source": "todo"}; self.secure_write(grant_path, json.dumps(grant, sort_keys=True)); self.manifest["humanFacingGrantDigest"] = hashlib.sha256(grant_path.read_bytes()).hexdigest(); self.write_manifest()
        with self.assertRaisesRegex(lifecycle.LifecycleError, "exactly bound"): self.validated()

    def test_authority_files_reject_modes_symlinks_and_writable_parents(self):
        self.manifest_path.chmod(0o644)
        with self.assertRaisesRegex(lifecycle.LifecycleError, "mode"): self.validated()
        self.manifest_path.chmod(0o600)
        with mock.patch.object(lifecycle.os, "geteuid", return_value=lifecycle.os.geteuid() + 1):
            with self.assertRaisesRegex(lifecycle.LifecycleError, "owner-controlled|owned"):
                self.validated()
        link = self.role_dir / "prompt-link"; link.symlink_to(self.prompt)
        value = self.activation_value(); value["promptPath"] = str(link); self.write_activation(value)
        with self.assertRaises(lifecycle.LifecycleError): lifecycle.load_activation(self.validated(), self.root)
        link.unlink(); self.role_dir.chmod(0o777)
        with self.assertRaisesRegex(lifecycle.LifecycleError, "writable"): self.validated()

    def test_activation_binds_task_routes_issuer_and_prompt_digest(self):
        role = self.validated()
        for field, bad in (("canonicalTask", {"id": "other", "source": "todo"}), ("senderRoute", self.parent), ("parentRoute", self.issuer), ("promptDigest", "0" * 64)):
            value = self.activation_value(); value[field] = bad; self.write_activation(value)
            with self.assertRaises(lifecycle.LifecycleError, msg=field): lifecycle.load_activation(role, self.root)

    def test_run_records_bound_identity_starts_once_and_hibernates(self):
        role = self.validated(); lifecycle.lifecycle_run(role, self.root, 0.001, 1)
        final = json.loads((Path(role["stateDir"]) / "activation-receipt.json").read_text())
        self.assertEqual(final["phase"], "completed"); self.assertTrue(final["hibernated"]); self.assertEqual(final["gateAdmission"], "unknown")
        self.assertIn("activationDigest", final); self.assertIn("promptDigest", final); self.assertIn("generation", final)
        events = self.events.read_text().splitlines(); self.assertEqual(sum(x == "herdr agent start" for x in events), 1); self.assertIn("herdr agent send-keys", events)

    def test_malformed_start_response_rolls_back_exact_generation(self):
        self.secure_write(self.mode, "start_malformed")
        role = self.validated()
        with self.assertRaisesRegex(lifecycle.LifecycleError, "rollback completed"): lifecycle.lifecycle_run(role, self.root, 0.001, 1)
        rollback = json.loads((Path(role["stateDir"]) / "rollback-receipt.json").read_text())
        self.assertEqual(rollback["disposition"], "completed"); self.assertIn("generation", rollback); self.assertIn("preStartIdentity", rollback)
        self.assertFalse((Path(role["stateDir"]) / "relaunch-inhibit.json").exists())

    def test_transport_parse_failure_rolls_back(self):
        self.secure_write(self.mode, "transport_malformed")
        role = self.validated()
        with self.assertRaisesRegex(lifecycle.LifecycleError, "rollback completed"): lifecycle.lifecycle_run(role, self.root, 0.001, 1)
        self.assertEqual(json.loads((Path(role["stateDir"]) / "rollback-receipt.json").read_text())["disposition"], "completed")

    def test_uncertain_cleanup_durably_inhibits_automatic_relaunch(self):
        self.secure_write(self.mode, "cleanup_uncertain")
        role = self.validated()
        with self.assertRaisesRegex(lifecycle.LifecycleError, "rollback uncertain"): lifecycle.lifecycle_run(role, self.root, 0.001, 1)
        state = Path(role["stateDir"])
        self.assertEqual(json.loads((state / "rollback-receipt.json").read_text())["disposition"], "uncertain")
        self.assertTrue((state / "relaunch-inhibit.json").exists())
        with self.assertRaises(lifecycle.LifecycleInhibited): lifecycle.lifecycle_run(role, self.root, 0.001, 1)

    def test_existing_live_process_is_never_ambiguously_attached(self):
        fake_state = self.root / "fake-state.json"; fake_state.write_text(json.dumps({"phase": "started", "name": "old", "gets": 0}))
        role = self.validated()
        with self.assertRaises(lifecycle.LifecycleInhibited): lifecycle.lifecycle_run(role, self.root, 0.001, 1)
        self.assertTrue((Path(role["stateDir"]) / "relaunch-inhibit.json").exists())

    def test_unit_remains_bounded_and_no_watcher_exists(self):
        unit = lifecycle.render_unit(self.validated(), MODULE_PATH.resolve())
        for value in ("Type=notify", "Restart=on-failure", "RestartSec=15s", "StartLimitBurst=3", "WatchdogSec=120s"): self.assertIn(value, unit)
        source = MODULE_PATH.read_text(); self.assertNotIn("inotify", source); self.assertNotIn("gate_inbox", source)
        self.assertLess(source.index('run_json([herdr, "agent", "prompt"'), source.index('notify(role, "READY=1"'))


if __name__ == "__main__": unittest.main()
