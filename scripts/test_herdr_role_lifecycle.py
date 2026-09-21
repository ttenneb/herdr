import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import tempfile
import threading
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
        self.herdr_socket = self.root / "herdr.sock"; self.herdr_socket_listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); self.herdr_socket_listener.bind(str(self.herdr_socket)); self.herdr_socket.chmod(0o600)
        self.addCleanup(self.herdr_socket_listener.close)
        self.herdr = self.executable("herdr", self.fake_herdr())
        self.systemctl = self.executable("systemctl", "#!/bin/sh\nexit 0\n")
        self.session = {"agent": "pi", "kind": "path", "source": "herdr:pi", "value": str(self.root / "session.jsonl")}
        self.issuer = self.route("issuer", "wi", "wi:p1", "termi")
        self.parent = self.route("parent", "wp", "wp:p1", "termp")
        self.tasking_parent = {key: self.parent[key] for key in ("workspaceId", "paneId", "terminalId", "agentSession")}
        self.parent_assignment = {"paneId": "wp:p1", "workspaceId": "wp", "agent": "pi", "agentSession": self.session, "assignedByPaneId": "controller:p1", "boundAt": "2026-09-21T04:00:00Z"}
        self.prompt = self.root / "prompt.txt"; self.secure_write(self.prompt, "Do the exact bounded assignment.\n")
        self.activation = self.role_dir / "activation.json"
        self.ack_socket = self.root / "a.sock"
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
        return {"version": 1, "roleId": "owner-1", "roleClass": "implementation-owner", "workspace": {"id": "w1", "path": str(self.worktree)}, "paneId": "w1:p2", "terminalId": "term2", "agentSession": self.session, "task": {"id": "100", "source": "todo"}, "mailboxPath": str(self.root / "mailbox.jsonl"), "reportRoute": self.parent, "authorizedIssuer": self.issuer, "stateDir": str(self.role_dir / "state"), "activationPath": str(self.activation), "executables": {"herdr": str(self.herdr), "pi": str(self.pi), "python": str(self.python), "systemdNotify": str(self.notify), "systemctl": str(self.systemctl)}, "herdrSocketPath": str(self.herdr_socket), "humanFacing": False, "reportAcknowledgement": {"endpointPath": str(self.ack_socket), "delegationId": "CCCCCCCCCCCCCCCCCCCCCC", "parentTaskId": 100, "parentAssignment": self.parent_assignment, "parentRoute": self.tasking_parent}}

    def activation_value(self):
        prompt_digest = hashlib.sha256(self.prompt.read_bytes()).hexdigest()
        return {"version": 1, "roleId": "owner-1", "executionId": "exec-1", "kind": "assignment", "canonicalTask": self.manifest["task"], "issuer": self.issuer, "senderRoute": self.issuer, "parentRoute": self.parent, "promptPath": str(self.prompt), "promptDigest": prompt_digest}

    def parent_ack_request(self, acknowledgement_id="abcdef0123456789abcdef0123456789"):
        return {"kind": "pi-tasking.report-parent-acknowledgement", "version": 1, "acknowledgementId": acknowledgement_id, "target": lifecycle.role_route(self.validated()), "attemptId": "AAAAAAAAAAAAAAAAAAAAAA", "reportId": "BBBBBBBBBBBBBBBBBBBBBB", "delegationId": "CCCCCCCCCCCCCCCCCCCCCC", "parentTaskId": 100, "sequence": 1, "sha256": "1" * 64, "parentAssignment": self.parent_assignment, "parentRoute": self.tasking_parent, "acknowledgedBy": self.tasking_parent, "receiptId": "parent-receipt-1", "confirmedAt": "2026-09-21T05:00:00Z"}

    def start_ack_listener(self, response_factory, frames, durable_marker=None, mode=0o600):
        ready = threading.Event()
        def serve():
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
                listener.bind(str(self.ack_socket)); self.ack_socket.chmod(mode); listener.listen(1); ready.set()
                connection, _ = listener.accept()
                with connection:
                    data = b""
                    while not data.endswith(b"\n"): data += connection.recv(65536)
                    frame = json.loads(data); frames.append(frame)
                    if durable_marker is not None: self.secure_write(durable_marker, json.dumps({"acknowledgementId": frame["acknowledgementId"]}))
                    response = response_factory(frame)
                    connection.sendall(json.dumps(response, sort_keys=True).encode() + b"\n")
            self.ack_socket.unlink(missing_ok=True)
        thread = threading.Thread(target=serve, daemon=True); thread.start(); self.assertTrue(ready.wait(2)); return thread

    def queued_request(self, activation_id="0123456789abcdef0123456789abcdef", batch_id="batch-1", correlation=None, depth=0):
        payload = "Queued explicit input."
        return {"kind": "pi-input-gate.queued-input-activation", "version": 1, "activationId": activation_id, "batchId": batch_id, "itemIds": ["item-1"], "priority": "normal", "correlation": correlation or [], "depth": depth, "cause": "accepted_queued_input", "payload": payload, "payloadSha256": hashlib.sha256(payload.encode()).hexdigest(), "recipient": lifecycle.exact_lifecycle_recipient(self.validated())}

    def write_activation(self, value=None): self.secure_write(self.activation, json.dumps(value or self.activation_value(), sort_keys=True))
    def write_manifest(self): self.secure_write(self.manifest_path, json.dumps(self.manifest, sort_keys=True))
    def validated(self): return lifecycle.load_manifest(self.manifest_path, self.root)

    def fake_herdr(self):
        state = self.root / "fake-state.json"
        return f'''#!/usr/bin/python3
import json,os,pathlib,sys
state=pathlib.Path({str(state)!r}); mode=pathlib.Path({str(self.mode)!r}).read_text().strip(); events=pathlib.Path({str(self.events)!r})
session={{"agent":"pi","kind":"path","source":"herdr:pi","value":{str(self.root / "session.jsonl")!r}}}
def load(): return json.loads(state.read_text()) if state.exists() else {{"phase":"empty","name":None,"gets":0}}
def save(x): state.write_text(json.dumps(x))
def emit(x): print(json.dumps({{"id":"fake","result":x}}))
def agent(s,status,ready=True): return {{"workspace_id":"w1","pane_id":"w1:p2","terminal_id":"term2","cwd":{str(self.worktree)!r},"agent":"pi" if s["phase"]!="empty" else None,"agent_session":session if s["phase"]!="empty" else None,"interactive_ready":ready,"name":s.get("name"),"agent_status":status,"state_change_seq":s.get("gets",0)+1,"revision":9}}
a=sys.argv[1:]; events.parent.mkdir(parents=True,exist_ok=True)
if mode=="require_socket" and os.environ.get("HERDR_SOCKET_PATH")!={str(self.herdr_socket)!r}: print("wrong socket",file=sys.stderr); sys.exit(7)
with events.open("a") as f: f.write("herdr "+" ".join(a[:2])+"\\n")
s=load()
if a[:2]==["agent","get"]:
 if len(a)>2 and a[2]=="wp:p1":
  emit({{"agent":{{"workspace_id":"wp","pane_id":"wp:p1","terminal_id":"termp","cwd":{str(self.worktree)!r},"agent":"pi","agent_session":session,"interactive_ready":True,"name":"parent","agent_status":"idle","state_change_seq":1,"revision":1}}}}); sys.exit(0)
 if mode in ("production_agent_not_found","production_agent_not_found_bad_pane") and s["phase"]=="empty":
  print(json.dumps({{"id":"cli:agent:get","error":{{"code":"agent_not_found","message":"agent not found"}}}}),file=sys.stderr); sys.exit(1)
 if mode=="cleanup_uncertain" and s["phase"]=="started" and s.get("cleanup_failed"): print("bad"); sys.exit(0)
 if mode in ("production_hibernate_not_found","production_hibernate_other_error") and s["phase"]=="exited":
  code="agent_not_found" if mode=="production_hibernate_not_found" else "server_not_running"; print(json.dumps({{"id":"cli:agent:get","error":{{"code":code,"message":"post-hibernate observation"}}}}),file=sys.stderr); sys.exit(1)
 if s["phase"]=="empty": emit({{"agent":agent(s,"unknown",False)}})
 elif s["phase"]=="exited": emit({{"agent":agent({{"phase":"empty","name":None,"gets":s["gets"]}},"unknown",False)}})
 else:
  s["gets"]+=1; save(s); emit({{"agent":agent(s,"working" if s["gets"]==1 else "idle")}})
elif a[:2]==["pane","get"]:
 pane=agent(s,"unknown",False); pane["terminal_id"]="wrong" if mode=="production_agent_not_found_bad_pane" else "term2"; emit({{"pane":pane}})
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

    def test_production_agent_not_found_preflight_accepts_only_exact_hibernated_pane(self):
        self.secure_write(self.mode, "production_agent_not_found")
        role = self.validated(); preflight = lifecycle.preflight_agent_state(role)
        self.assertIsNone(preflight["agent"]); self.assertEqual(preflight["terminal_id"], "term2")
        lifecycle.lifecycle_run(role, self.root, 0.001, 1)
        final = json.loads((Path(role["stateDir"]) / "activation-receipt.json").read_text())
        self.assertEqual(final["phase"], "completed"); self.assertTrue(final["hibernated"])
        events = self.events.read_text().splitlines(); self.assertLess(events.index("herdr agent get"), events.index("herdr pane get")); self.assertLess(events.index("herdr pane get"), events.index("herdr agent start"))

        shutil.rmtree(role["stateDir"]); (self.root / "fake-state.json").unlink(); self.secure_write(self.mode, "production_agent_not_found_bad_pane")
        with self.assertRaisesRegex(lifecycle.LifecycleError, "terminal_id mismatch"):
            lifecycle.lifecycle_run(role, self.root, 0.001, 1)
        self.assertEqual(self.events.read_text().splitlines().count("herdr agent start"), 1)

    def test_post_hibernate_accepts_only_production_agent_not_found(self):
        role = self.validated(); fake_state = self.root / "fake-state.json"
        fake_state.write_text(json.dumps({"phase": "started", "name": "exact-generation", "gets": 0}))
        self.secure_write(self.mode, "production_hibernate_not_found")
        lifecycle.exact_hibernate(role, "exact-generation", timeout=0.2)
        events = self.events.read_text().splitlines(); self.assertLess(events.index("herdr agent send-keys"), events.index("herdr agent get"))

        fake_state.write_text(json.dumps({"phase": "started", "name": "exact-generation", "gets": 0}))
        self.secure_write(self.mode, "production_hibernate_other_error")
        with self.assertRaisesRegex(lifecycle.LifecycleError, "server_not_running"):
            lifecycle.exact_hibernate(role, "exact-generation", timeout=0.2)
        self.assertEqual(self.events.read_text().splitlines().count("herdr agent send-keys"), 2)

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

    def test_frozen_queued_input_v1_fixture_matches_exact_contract(self):
        fixture_path = Path(__file__).parents[1] / "tests/fixtures/queued_input_activation_request_v1.json"
        fixture = json.loads(fixture_path.read_text())
        self.assertEqual(set(fixture), {"kind", "version", "activationId", "batchId", "itemIds", "priority", "correlation", "depth", "cause", "payload", "payloadSha256", "recipient"})
        self.assertEqual(hashlib.sha256(fixture["payload"].encode()).hexdigest(), fixture["payloadSha256"])
        fixture["recipient"] = lifecycle.exact_lifecycle_recipient(self.validated())
        lifecycle.validate_queued_input_request(self.validated(), fixture)

    def test_queued_input_schedules_once_and_exact_duplicate_does_not_prompt(self):
        role = self.validated(); request = self.queued_request()
        first = lifecycle.schedule_queued_input(role, request, self.root, self.issuer)
        self.assertEqual(first["outcome"], "scheduled")
        record_path, activation_path, payload_path = lifecycle.queue_record_paths(role, request["activationId"])
        before = (activation_path.read_bytes(), payload_path.read_bytes(), activation_path.stat().st_mtime_ns)
        second = lifecycle.schedule_queued_input(role, request, self.root, self.issuer)
        self.assertEqual(second["outcome"], "duplicate"); self.assertEqual(second["receiptId"], first["receiptId"])
        self.assertEqual(before, (activation_path.read_bytes(), payload_path.read_bytes(), activation_path.stat().st_mtime_ns))
        activation = json.loads(activation_path.read_text())
        self.assertEqual(activation["cause"], "accepted_queued_input"); self.assertEqual(activation["issuer"], self.issuer)
        self.assertEqual(activation["queuedInputActivation"]["recipient"], lifecycle.exact_lifecycle_recipient(role))
        self.assertFalse(self.events.exists(), "scheduling must not start a process or submit a prompt")
        self.assertEqual(json.loads(record_path.read_text())["outcome"], "scheduled")

    def test_scheduled_queued_activation_runs_once_through_external_lifecycle(self):
        role = self.validated(); request = self.queued_request()
        self.assertEqual(lifecycle.schedule_queued_input(role, request, self.root, self.issuer)["outcome"], "scheduled")
        record_path, activation_path, _ = lifecycle.queue_record_paths(role, request["activationId"])
        lifecycle.lifecycle_run(role, self.root, 0.001, 1, activation_path)
        record = json.loads(record_path.read_text())
        self.assertFalse(record["active"]); self.assertEqual(record["lifecycleOutcome"], "completed")
        events = self.events.read_text().splitlines()
        self.assertEqual(sum(line == "herdr agent start" for line in events), 1)
        self.assertEqual(sum(line == "herdr agent prompt" for line in events), 1)

    def test_same_id_conflict_is_rejected(self):
        role = self.validated(); request = self.queued_request()
        self.assertEqual(lifecycle.schedule_queued_input(role, request, self.root, self.issuer)["outcome"], "scheduled")
        conflict = dict(request); conflict["batchId"] = "batch-2"
        result = lifecycle.schedule_queued_input(role, conflict, self.root, self.issuer)
        self.assertEqual(result["outcome"], "rejected"); self.assertIn("conflicts", result["reason"])

    def test_lost_ack_stays_uncertain_until_same_id_recovery_without_rematerializing(self):
        role = self.validated(); request = self.queued_request()
        first = lifecycle.schedule_queued_input(role, request, self.root, self.issuer, fault="lost_ack")
        self.assertEqual(first["outcome"], "uncertain")
        _, activation_path, _ = lifecycle.queue_record_paths(role, request["activationId"])
        before = (activation_path.read_bytes(), activation_path.stat().st_mtime_ns)
        with self.assertRaisesRegex(lifecycle.LifecycleError, "scheduled record"):
            lifecycle.load_activation(role, self.root, activation_path)
        retry = lifecycle.schedule_queued_input(role, request, self.root, self.issuer)
        self.assertEqual(retry["outcome"], "uncertain")
        start_calls = []
        def start_runner(command, **kwargs):
            start_calls.append(command); return __import__("subprocess").CompletedProcess(command, 0, "", "")
        blocked_start = lifecycle.start_queued_input_service(role, request["activationId"], self.root, runner=start_runner)
        self.assertEqual(blocked_start["outcome"], "rejected"); self.assertEqual(start_calls, [])
        recovered = lifecycle.schedule_queued_input(role, request, self.root, self.issuer, recover=True)
        self.assertEqual(recovered["outcome"], "duplicate"); self.assertIn("no second activation", recovered["reason"])
        self.assertEqual(before, (activation_path.read_bytes(), activation_path.stat().st_mtime_ns))
        lifecycle.load_activation(role, self.root, activation_path)
        started = lifecycle.start_queued_input_service(role, request["activationId"], self.root, runner=start_runner)
        self.assertEqual(started["outcome"], "accepted"); self.assertEqual(len(start_calls), 1)
        self.assertFalse(self.events.exists())

    def test_queued_input_rejects_noncauses_digest_bounds_and_wrong_recipient(self):
        role = self.validated()
        unauthorized = dict(self.issuer); unauthorized["terminalId"] = "other"
        denied = lifecycle.schedule_queued_input(role, self.queued_request(), self.root, unauthorized)
        self.assertEqual(denied["outcome"], "rejected"); self.assertIn("issuer", denied["reason"])
        cases = []
        for index, cause in enumerate(("empty_settlement", "transport_receipt", "report_import", "status", "heartbeat", "supervisor_observation"), start=10):
            wrong_cause = self.queued_request(f"{index:032x}"); wrong_cause["cause"] = cause; cases.append(wrong_cause)
        empty = self.queued_request("1123456789abcdef0123456789abcdef"); empty["payload"] = ""; empty["payloadSha256"] = hashlib.sha256(b"").hexdigest(); cases.append(empty)
        bad_digest = self.queued_request("2123456789abcdef0123456789abcdef"); bad_digest["payloadSha256"] = "0" * 64; cases.append(bad_digest)
        wrong_recipient = self.queued_request("3123456789abcdef0123456789abcdef"); wrong_recipient["recipient"] = dict(wrong_recipient["recipient"]); wrong_recipient["recipient"]["terminalId"] = "other"; cases.append(wrong_recipient)
        too_deep = self.queued_request("4123456789abcdef0123456789abcdef", depth=lifecycle.MAX_QUEUED_DEPTH + 1); cases.append(too_deep)
        too_many = self.queued_request("5123456789abcdef0123456789abcdef"); too_many["itemIds"] = [f"item-{i}" for i in range(33)]; cases.append(too_many)
        for request in cases:
            self.assertEqual(lifecycle.schedule_queued_input(role, request, self.root, self.issuer)["outcome"], "rejected")
        self.assertFalse(self.events.exists())

    def test_per_role_rate_queue_and_correlation_loop_guards(self):
        role = self.validated()
        first = self.queued_request(correlation=[{"namespace": "report", "key": "chain", "revision": 5}])
        self.assertEqual(lifecycle.schedule_queued_input(role, first, self.root, self.issuer)["outcome"], "scheduled")
        loop = self.queued_request("1123456789abcdef0123456789abcdef", "batch-2", [{"namespace": "report", "key": "chain", "revision": 5}])
        result = lifecycle.schedule_queued_input(role, loop, self.root, self.issuer)
        self.assertEqual(result["outcome"], "rejected"); self.assertIn("correlation loop", result["reason"])
        with mock.patch.object(lifecycle, "ROLE_QUEUE_LIMIT", 1):
            depth = self.queued_request("2123456789abcdef0123456789abcdef", "batch-3")
            self.assertIn("depth limit", lifecycle.schedule_queued_input(role, depth, self.root, self.issuer)["reason"])
        for index in range(1, lifecycle.ROLE_RATE_LIMIT):
            request = self.queued_request(f"{index + 3:032x}", f"rate-{index}")
            self.assertEqual(lifecycle.schedule_queued_input(role, request, self.root, self.issuer)["outcome"], "scheduled")
        limited = self.queued_request("f123456789abcdef0123456789abcdef", "rate-limit")
        rate = lifecycle.schedule_queued_input(role, limited, self.root, self.issuer)
        self.assertEqual(rate["outcome"], "rejected"); self.assertIn("rate limit", rate["reason"])

    def test_parent_ack_schema_and_authority_bindings(self):
        role = self.validated(); request = self.parent_ack_request()
        lifecycle.validate_parent_ack_request(role, request)
        fixture_path = Path(__file__).parents[1] / "tests/fixtures/report_parent_acknowledgement_v1.json"
        fixture = json.loads(fixture_path.read_text())
        long_value = fixture["parentRoute"]["agentSession"]["value"]
        self.assertGreater(len(long_value.encode()), 128); self.assertLessEqual(len(long_value.encode()), 512)
        long_manifest = json.loads(json.dumps(self.manifest)); long_manifest["reportRoute"] = {"name": "parent", **fixture["parentRoute"]}
        long_manifest["reportAcknowledgement"]["parentRoute"] = fixture["parentRoute"]
        long_manifest["reportAcknowledgement"]["parentAssignment"] = fixture["parentAssignment"]
        long_role = lifecycle.validate_manifest(long_manifest, self.manifest_path, self.root)
        fixture["target"] = lifecycle.role_route(long_role)
        lifecycle.validate_parent_ack_request(long_role, fixture)
        overlong_route = json.loads(json.dumps(fixture["parentRoute"])); overlong_route["agentSession"]["value"] = "x" * 513
        overlong_assignment = json.loads(json.dumps(fixture["parentAssignment"])); overlong_assignment["agentSession"]["value"] = "x" * 513
        with self.assertRaises(lifecycle.LifecycleError): lifecycle.exact_tasking_route(overlong_route, "test.route")
        with self.assertRaises(lifecycle.LifecycleError): lifecycle.exact_task_assignment(overlong_assignment, "test.assignment")
        optional_overlong = json.loads(json.dumps(fixture["parentAssignment"])); optional_overlong["assignedByPaneId"] = "x" * 129
        with self.assertRaises(lifecycle.LifecycleError): lifecycle.exact_task_assignment(optional_overlong, "test.assignment")
        def reject_live(*_): raise lifecycle.LifecycleError("live parent issuer route/session mismatch")
        live_mismatch = lifecycle.deliver_parent_acknowledgement(role, request, self.root, verify_parent=reject_live)
        self.assertEqual(live_mismatch["outcome"], "rejected"); self.assertIn("live parent", live_mismatch["reason"])
        assignment_extra = dict(self.parent_assignment, extra="forbidden")
        route_extra = dict(self.tasking_parent, name="forbidden")
        route_session_extra = json.loads(json.dumps(self.tasking_parent)); route_session_extra["agentSession"]["extra"] = "forbidden"
        for field, value in (("attemptId", "A" * 21), ("reportId", "B" * 23), ("delegationId", "other"), ("parentTaskId", 101), ("parentAssignment", assignment_extra), ("parentRoute", route_extra), ("acknowledgedBy", route_session_extra), ("acknowledgedBy", self.issuer), ("target", self.issuer), ("sha256", "0" * 63)):
            bad = dict(request); bad[field] = value
            result = lifecycle.deliver_parent_acknowledgement(role, bad, self.root)
            self.assertEqual(result["outcome"], "rejected", field)
        bad = dict(request); bad["extra"] = True
        self.assertEqual(lifecycle.deliver_parent_acknowledgement(role, bad, self.root)["outcome"], "rejected")
        bad_result = {"kind": "pi-tasking.report-parent-acknowledgement-result", "version": 1, "acknowledgementId": request["acknowledgementId"], "attemptId": request["attemptId"], "reportId": request["reportId"], "outcome": "uncertain", "extra": True}
        with self.assertRaises(lifecycle.LifecycleError): lifecycle.validate_parent_ack_result(request, bad_result)
        for field, value in (("delegationId", "short"), ("parentAssignment", assignment_extra), ("parentRoute", route_extra)):
            bad_manifest = json.loads(json.dumps(self.manifest)); bad_manifest["reportAcknowledgement"][field] = value
            with self.assertRaises(lifecycle.LifecycleError, msg=field):
                lifecycle.validate_manifest(bad_manifest, self.manifest_path, self.root)
        artifact = Path(__file__).parents[1] / "docs/next/report-parent-acknowledgement-v1.schema.json"
        self.assertEqual(json.loads(artifact.read_text()), lifecycle.parent_ack_schemas())

    def test_parent_ack_socket_delivery_requires_child_durability_and_dedupes_after_restart(self):
        role = self.validated(); request = self.parent_ack_request(); frames = []; marker = self.role_dir / "child-durable.json"
        def confirmed(frame):
            return {"kind": "pi-tasking.report-parent-acknowledgement-result", "version": 1, "acknowledgementId": request["acknowledgementId"], "attemptId": request["attemptId"], "reportId": request["reportId"], "outcome": "confirmed", "receiptId": "tasking-send-receipt-1"}
        thread = self.start_ack_listener(confirmed, frames, marker)
        result = lifecycle.deliver_parent_acknowledgement(role, request, self.root, verify_parent=lambda *_: None)
        thread.join(2); self.assertFalse(thread.is_alive()); self.assertTrue(marker.exists())
        self.assertEqual(result["outcome"], "confirmed"); self.assertEqual(len(frames), 1)
        restarted_role = self.validated()
        duplicate = lifecycle.deliver_parent_acknowledgement(restarted_role, request, self.root, verify_parent=lambda *_: None)
        self.assertEqual(duplicate["outcome"], "duplicate"); self.assertEqual(duplicate["receiptId"], "tasking-send-receipt-1")

    def test_parent_ack_cli_is_a_real_non_prompt_delivery_path(self):
        request = self.parent_ack_request(); request_path = self.role_dir / "ack-request.json"; self.secure_write(request_path, json.dumps(request))
        frames = []
        def confirmed(_frame):
            return {"kind": "pi-tasking.report-parent-acknowledgement-result", "version": 1, "acknowledgementId": request["acknowledgementId"], "attemptId": request["attemptId"], "reportId": request["reportId"], "outcome": "confirmed", "receiptId": "tasking-send-receipt-cli"}
        thread = self.start_ack_listener(confirmed, frames)
        process = __import__("subprocess").run([str(self.python), str(MODULE_PATH), "send-parent-ack", "--manifest", str(self.manifest_path), "--durable-root", str(self.root), "--request", str(request_path)], capture_output=True, text=True, timeout=10)
        thread.join(2)
        self.assertEqual(process.returncode, 0, process.stderr)
        self.assertEqual(json.loads(process.stdout)["outcome"], "confirmed"); self.assertEqual(len(frames), 1)
        self.assertEqual(frames[0]["kind"], "pi-tasking.report-parent-acknowledgement")

    def test_parent_ack_lost_result_stays_uncertain_and_recovery_only_queries(self):
        role = self.validated(); request = self.parent_ack_request(); frames = []
        def confirmed(_frame):
            return {"kind": "pi-tasking.report-parent-acknowledgement-result", "version": 1, "acknowledgementId": request["acknowledgementId"], "attemptId": request["attemptId"], "reportId": request["reportId"], "outcome": "confirmed", "receiptId": "tasking-send-receipt-1"}
        thread = self.start_ack_listener(confirmed, frames)
        uncertain = lifecycle.deliver_parent_acknowledgement(role, request, self.root, verify_parent=lambda *_: None, fault="lost_ack")
        thread.join(2); self.assertEqual(uncertain["outcome"], "uncertain")
        retry = lifecycle.deliver_parent_acknowledgement(role, request, self.root, verify_parent=lambda *_: None)
        self.assertEqual(retry["outcome"], "uncertain"); self.assertEqual(len(frames), 1)
        def duplicate(_frame):
            return {"kind": "pi-tasking.report-parent-acknowledgement-result", "version": 1, "acknowledgementId": request["acknowledgementId"], "attemptId": request["attemptId"], "reportId": request["reportId"], "outcome": "duplicate", "receiptId": "tasking-send-receipt-1"}
        thread = self.start_ack_listener(duplicate, frames)
        recovered = lifecycle.deliver_parent_acknowledgement(role, request, self.root, verify_parent=lambda *_: None, recover=True)
        thread.join(2); self.assertEqual(recovered["outcome"], "duplicate"); self.assertEqual(len(frames), 2)
        self.assertEqual(frames[0]["kind"], "pi-tasking.report-parent-acknowledgement")
        self.assertEqual(frames[1]["kind"], "pi-tasking.report-parent-acknowledgement-query")
        final = lifecycle.deliver_parent_acknowledgement(role, request, self.root, verify_parent=lambda *_: None)
        self.assertEqual(final["outcome"], "duplicate"); self.assertEqual(len(frames), 2)

    def test_parent_ack_endpoint_is_owner_only_and_non_model(self):
        role = self.validated()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
            listener.bind(str(self.ack_socket)); self.ack_socket.chmod(0o666)
            with self.assertRaisesRegex(lifecycle.LifecycleError, "owner-only"):
                lifecycle.validate_ack_endpoint(self.ack_socket, self.root)
        self.ack_socket.unlink(missing_ok=True)
        source = __import__("inspect").getsource(lifecycle.deliver_parent_acknowledgement)
        for forbidden in ("agent prompt", "send-keys", "editor", "PTY", "sendUserMessage"):
            self.assertNotIn(forbidden, source)

    def test_queued_unit_pins_exact_activation_id_and_escapes_paths(self):
        role = self.validated()
        special_manager = self.root / 'manager % "quoted".py'; self.secure_write(special_manager, "# manager\n")
        role["_manifestPath"] = str(self.root / 'role/manifest % "quoted".json')
        unit = lifecycle.render_queued_unit(role, special_manager)
        self.assertIn("Type=notify", unit); self.assertIn("NotifyAccess=main", unit); self.assertIn("Restart=on-failure", unit)
        self.assertIn("RestartSec=15s", unit); self.assertIn("WatchdogSec=120s", unit); self.assertIn("StartLimitBurst=3", unit)
        self.assertIn(f'Environment="HERDR_SOCKET_PATH={self.herdr_socket}"', unit)
        self.assertTrue(unit.startswith("# UnitName=herdr-role-owner-1@.service\n"))
        self.assertIn("--activation-id %i", unit); self.assertNotIn("WantedBy=", unit)
        self.assertIn('%%', unit); self.assertIn('\\"quoted\\"', unit)
        self.assertEqual(lifecycle.queued_unit_template_name(role), "herdr-role-owner-1@.service")

    def test_service_start_requires_scheduled_exact_id_and_is_idempotent(self):
        role = self.validated(); activation_id = "0123456789abcdef0123456789abcdef"
        calls = []
        def runner(command, **kwargs):
            calls.append((command, kwargs)); return __import__("subprocess").CompletedProcess(command, 0, "", "")
        rejected = lifecycle.start_queued_input_service(role, activation_id, self.root, runner=runner)
        self.assertEqual(rejected["outcome"], "rejected"); self.assertEqual(calls, [])
        request = self.queued_request(activation_id)
        self.assertEqual(lifecycle.schedule_queued_input(role, request, self.root, self.issuer)["outcome"], "scheduled")
        started = lifecycle.start_queued_input_service(role, activation_id, self.root, runner=runner)
        self.assertEqual(started["outcome"], "accepted"); self.assertEqual(len(calls), 1)
        expected_unit = lifecycle.queued_unit_instance_name(role, activation_id)
        self.assertEqual(calls[0][0], [str(self.systemctl), "--user", "start", expected_unit])
        duplicate = lifecycle.start_queued_input_service(role, activation_id, self.root, runner=runner)
        self.assertEqual(duplicate["outcome"], "duplicate"); self.assertEqual(len(calls), 1)

    def test_invalid_or_generic_service_start_has_no_start_path(self):
        role = self.validated(); calls = []
        def runner(command, **kwargs): calls.append(command); raise AssertionError("must not run")
        for activation_id in ("", "not-an-id", "0" * 31, "../../generic"):
            result = lifecycle.start_queued_input_service(role, activation_id, self.root, runner=runner)
            self.assertEqual(result["outcome"], "rejected")
        self.assertEqual(calls, [])

    def test_service_start_lost_ack_stays_uncertain_until_explicit_recovery(self):
        role = self.validated(); request = self.queued_request()
        lifecycle.schedule_queued_input(role, request, self.root, self.issuer)
        calls = []
        def runner(command, **kwargs):
            calls.append(command); return __import__("subprocess").CompletedProcess(command, 0, "", "")
        first = lifecycle.start_queued_input_service(role, request["activationId"], self.root, runner=runner, fault="lost_ack")
        self.assertEqual(first["outcome"], "uncertain"); self.assertEqual(len(calls), 1)
        retry = lifecycle.start_queued_input_service(role, request["activationId"], self.root, runner=runner)
        self.assertEqual(retry["outcome"], "uncertain"); self.assertEqual(len(calls), 1)
        recovered = lifecycle.recover_queued_input_start(role, request["activationId"], "started")
        self.assertEqual(recovered["outcome"], "duplicate"); self.assertEqual(len(calls), 1)
        final = lifecycle.start_queued_input_service(role, request["activationId"], self.root, runner=runner)
        self.assertEqual(final["outcome"], "duplicate"); self.assertEqual(len(calls), 1)

    def test_generated_queued_start_schema_is_current(self):
        expected = lifecycle.queued_service_start_schema()
        artifact = Path(__file__).parents[1] / "docs/next/queued-input-service-start-v1.schema.json"
        self.assertEqual(json.loads(artifact.read_text()), expected)

    def test_manifest_bound_herdr_socket_is_secure_and_propagated(self):
        role = self.validated(); self.herdr_socket.chmod(0o666)
        with self.assertRaisesRegex(lifecycle.LifecycleError, "owner-only"):
            self.validated()
        self.herdr_socket.chmod(0o600)
        link = self.root / "herdr-link.sock"; link.symlink_to(self.herdr_socket)
        self.manifest["herdrSocketPath"] = str(link); self.write_manifest()
        with self.assertRaisesRegex(lifecycle.LifecycleError, "owner-only"):
            self.validated()
        self.manifest["herdrSocketPath"] = str(self.herdr_socket); self.write_manifest(); role = self.validated()
        self.secure_write(self.mode, "require_socket")
        with mock.patch.dict(os.environ, {"HERDR_SOCKET_PATH": "/wrong/ambient.sock"}):
            lifecycle.verify_live_parent_issuer(role, self.tasking_parent)
            lifecycle.lifecycle_run(role, self.root, 0.001, 1)
        unit = lifecycle.render_unit(role, MODULE_PATH.resolve())
        self.assertIn(f'Environment="HERDR_SOCKET_PATH={self.herdr_socket}"', unit)
        self.assertEqual(json.loads((Path(role["stateDir"]) / "activation-receipt.json").read_text())["phase"], "completed")

    def test_unit_remains_bounded_and_no_watcher_exists(self):
        unit = lifecycle.render_unit(self.validated(), MODULE_PATH.resolve())
        for value in ("Type=notify", "Restart=on-failure", "RestartSec=15s", "StartLimitBurst=3", "WatchdogSec=120s"): self.assertIn(value, unit)
        source = MODULE_PATH.read_text(); self.assertNotIn("inotify", source); self.assertNotIn("gate_inbox", source)
        self.assertLess(source.index('run_json([herdr, "agent", "prompt"'), source.index('notify(role, "READY=1"'))


if __name__ == "__main__": unittest.main()
