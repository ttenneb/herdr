//! #145 covered-child launch: the pinned bubblewrap sandbox policy and the
//! kernel enforcement verifier used by the #159 closure domain.
//!
//! The policy is a Rust port of the #145 packet's `sandbox_policy.py` (the
//! source the second live proof passed with), with one carried-over change:
//! the child inherits exactly the two allowlisted HERDR_* names instead of
//! the whole prefix. The server never trusts a caller-supplied receipt; it
//! computes the argv hash itself and the verifier re-derives every fact from
//! the kernel.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

use crate::child_report_closure::{
    BirthRecord, CoveredLaunchPolicy, EnforcementAttestation, EnforcementQuery, EnforcementVerifier,
};

pub const POLICY_ID: &str = "herdr-covered-bwrap-v1";
pub const BWRAP: &str = "/usr/bin/bwrap";
/// Exactly the namespaces the #145 proof used; no cgroup and no --new-session.
pub const NAMESPACE_FLAGS: [&str; 6] = [
    "--unshare-user",
    "--unshare-net",
    "--unshare-ipc",
    "--unshare-uts",
    "--unshare-pid",
    "--die-with-parent",
];
const FORBIDDEN_ENV_PREFIXES: [&str; 2] = ["JITI_", "NODE_"];
const FORBIDDEN_ENV_EXACT: [&str; 3] = ["NODE_OPTIONS", "NODE_PATH", "PI_PACKAGE_DIR"];
const REQUIRED_FIXED_ENV: [(&str, &str); 2] = [
    ("JITI_FS_CACHE", "false"),
    ("NODE_DISABLE_COMPILE_CACHE", "1"),
];

/// Caller-selected directories for one covered launch. Every path must be an
/// existing canonical absolute path; binds must stay under `root`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CoveredLaunchParams {
    /// Private candidate root that contains every bind.
    pub root: String,
    /// Child working directory; must lie in a writable bind.
    pub cwd: String,
    /// Absolute command replacing the agent executable, e.g. node and Pi's
    /// cli.js. The agent arguments follow it.
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lock_dirs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readonly: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyError(pub &'static str);

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

fn canonical_abs(path: &str) -> Result<String, PolicyError> {
    let candidate = Path::new(path);
    if !candidate.is_absolute()
        || candidate
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        || path.contains('\0')
    {
        return Err(PolicyError("policy_path_not_canonical_absolute"));
    }
    match std::fs::canonicalize(candidate) {
        Ok(resolved) if resolved == candidate => Ok(path.to_owned()),
        _ => Err(PolicyError("policy_path_not_canonical_absolute")),
    }
}

/// Exact child environment: fixed values plus the two allowlisted HERDR_*
/// names from the server's own launch environment.
pub fn child_env(
    root: &str,
    herdr_env: impl IntoIterator<Item = (String, String)>,
) -> Result<BTreeMap<String, String>, PolicyError> {
    let root = canonical_abs(root)?;
    let mut env: BTreeMap<String, String> = [
        ("HOME", format!("{root}/home")),
        ("XDG_CONFIG_HOME", format!("{root}/home/.config")),
        ("XDG_STATE_HOME", format!("{root}/home/.local/state")),
        ("XDG_CACHE_HOME", format!("{root}/home/.cache")),
        ("PI_CODING_AGENT_DIR", format!("{root}/agent")),
        ("PATH", "/usr/bin".into()),
        ("LANG", "C.UTF-8".into()),
        ("LC_ALL", "C.UTF-8".into()),
        ("TERM", "xterm-256color".into()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect();
    for (key, value) in REQUIRED_FIXED_ENV {
        env.insert(key.into(), value.into());
    }
    env.extend(crate::child_report_closure::covered_child_herdr_environment(herdr_env));
    verify_env(&env)?;
    Ok(env)
}

pub fn verify_env(env: &BTreeMap<String, String>) -> Result<(), PolicyError> {
    for (key, value) in REQUIRED_FIXED_ENV {
        if env.get(key).map(String::as_str) != Some(value) {
            return Err(PolicyError("required_cache_env_missing"));
        }
    }
    for key in env.keys() {
        let fixed = REQUIRED_FIXED_ENV.iter().any(|(fixed, _)| fixed == key);
        if FORBIDDEN_ENV_EXACT.contains(&key.as_str())
            || (!fixed && FORBIDDEN_ENV_PREFIXES.iter().any(|p| key.starts_with(p)))
        {
            return Err(PolicyError("forbidden_env_present"));
        }
    }
    Ok(())
}

/// Build the exact bwrap argv. The order is fixed so its SHA-256 is the
/// policy hash. `sockets` are server-chosen (the mailbox bootstrap socket).
pub fn bwrap_argv(
    params: &CoveredLaunchParams,
    sockets: &[String],
    env: &BTreeMap<String, String>,
    agent_args: &[String],
    uid: u32,
    gid: u32,
) -> Result<Vec<String>, PolicyError> {
    let root = canonical_abs(&params.root)?;
    let cwd = canonical_abs(&params.cwd)?;
    let canonical = |paths: &[String]| -> Result<Vec<String>, PolicyError> {
        paths.iter().map(|path| canonical_abs(path)).collect()
    };
    let lock_dirs = canonical(&params.lock_dirs)?;
    let writable = canonical(&params.writable)?;
    let readonly = canonical(&params.readonly)?;
    let sockets = canonical(sockets)?;
    let Some(executable) = params.command.first() else {
        return Err(PolicyError("command_missing"));
    };
    if !Path::new(executable).is_absolute()
        || params
            .command
            .iter()
            .chain(agent_args)
            .any(|arg| arg.contains('\0'))
    {
        return Err(PolicyError("command_not_absolute"));
    }
    let mut argv: Vec<String> = [BWRAP]
        .into_iter()
        .chain(NAMESPACE_FLAGS)
        .map(str::to_owned)
        .collect();
    argv.extend([
        "--uid".into(),
        uid.to_string(),
        "--gid".into(),
        gid.to_string(),
    ]);
    for part in [
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        "--symlink",
        "usr/sbin",
        "/sbin",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib64",
        "/lib64",
        "--ro-bind",
        "/etc",
        "/etc",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
    ] {
        argv.push(part.into());
    }
    let protected: Vec<&String> = lock_dirs.iter().chain(&readonly).collect();
    for w in &writable {
        if protected
            .iter()
            .any(|p| *p == w || p.starts_with(&format!("{w}/")))
        {
            return Err(PolicyError("writable_bind_would_shadow_readonly"));
        }
    }
    // Writable first, read-only last: later mounts win.
    for (group, flag) in [
        (&writable, "--bind"),
        (&readonly, "--ro-bind"),
        (&lock_dirs, "--ro-bind"),
    ] {
        for path in group {
            if !path.starts_with(&format!("{root}/")) {
                return Err(PolicyError("bind_outside_candidate_root"));
            }
            argv.extend([flag.to_owned(), path.clone(), path.clone()]);
        }
    }
    for socket in &sockets {
        argv.extend(["--bind".to_owned(), socket.clone(), socket.clone()]);
    }
    if !writable
        .iter()
        .any(|w| &cwd == w || cwd.starts_with(&format!("{w}/")))
    {
        return Err(PolicyError("cwd_not_writable_bind"));
    }
    argv.extend(["--chdir".into(), cwd, "--clearenv".into()]);
    verify_env(env)?;
    for (key, value) in env {
        if key.contains('=') || key.contains('\0') || value.contains('\0') {
            return Err(PolicyError("env_not_encodable"));
        }
        argv.extend(["--setenv".into(), key.clone(), value.clone()]);
    }
    argv.push("--".into());
    argv.extend(params.command.iter().cloned());
    argv.extend(agent_args.iter().cloned());
    Ok(argv)
}

pub fn policy_hash(argv: &[String]) -> String {
    format!("{:x}", sha2::Sha256::digest(argv.join("\0").as_bytes()))
}

/// Server-authored receipt naming the exact managed-launch birth.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoveredLaunchReceipt {
    pub kind: &'static str,
    pub version: u8,
    pub policy_id: &'static str,
    pub argv_sha256: String,
    pub env_keys: Vec<String>,
    pub launch_birth: BirthRecord,
    pub launch_floor_ticks: u64,
}

impl CoveredLaunchReceipt {
    pub fn digest(&self) -> String {
        format!(
            "{:x}",
            sha2::Sha256::digest(serde_json::to_vec(self).unwrap_or_default())
        )
    }

    pub fn policy(&self) -> CoveredLaunchPolicy {
        CoveredLaunchPolicy {
            policy_id: self.policy_id.into(),
            policy_hash: self.argv_sha256.clone(),
            receipt_digest: self.digest(),
            sandboxed_birth: self.launch_birth,
        }
    }
}

/// A covered launch awaiting its managed-launch birth (bound at Active).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCoveredLaunch {
    pub generation: u64,
    pub argv_sha256: String,
    pub env_keys: Vec<String>,
}

/// Kernel facts about one process, never taken from Herdr's own frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcView {
    pub pid: u32,
    pub birth: u64,
    pub ppid: u32,
    pub pgrp: u32,
    pub nspid: Vec<u32>,
    pub pidns: String,
    pub argv: Vec<String>,
    pub environ: BTreeMap<String, String>,
    pub exe: (u64, u64),
}

pub trait ProcSource: Send + Sync {
    fn view(&self, pid: u32) -> Option<ProcView>;
    fn self_pidns(&self) -> Option<String>;
    fn bwrap_exe(&self) -> Option<(u64, u64)>;
}

/// The exact environment bwrap establishes: `--setenv` pairs after
/// `--clearenv`, plus the PWD it exports after `--chdir`.
fn expected_environ(leader_argv: &[String]) -> Option<BTreeMap<String, String>> {
    let end = leader_argv.iter().position(|arg| arg == "--")?;
    let args = &leader_argv[..end];
    if !args.iter().any(|arg| arg == "--clearenv") {
        return None;
    }
    let mut env = BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--setenv" => {
                env.insert(args.get(index + 1)?.clone(), args.get(index + 2)?.clone());
                index += 3;
            }
            "--chdir" => {
                env.insert("PWD".into(), args.get(index + 1)?.clone());
                index += 2;
            }
            _ => index += 1,
        }
    }
    env.contains_key("PWD").then_some(env)
}

/// The #145 kernel rule as an enforcement verifier: the managed Pi is the
/// exact launch birth, lives in the PID namespace whose init is a bwrap
/// forked by the host bwrap group leader, that leader runs exactly the
/// hashed policy argv, the sandbox began at or after the launch floor and
/// before Pi, and Pi's environment is exactly the policy's. A process cannot
/// enter a PID namespace after birth, so Pi was covered from its exec.
pub struct KernelEnforcementVerifier<P: ProcSource> {
    pub proc: P,
}

impl<P: ProcSource> EnforcementVerifier for KernelEnforcementVerifier<P> {
    fn verify(&self, query: &EnforcementQuery<'_>) -> Result<EnforcementAttestation, String> {
        let birth = query.managed_launch_birth;
        let pi = self.proc.view(birth.pid).ok_or("pi_unreadable")?;
        if pi.birth != birth.start_ticks {
            return Err("launch_birth_mismatch".into());
        }
        if pi.nspid.len() < 2 || pi.nspid[0] != pi.pid || pi.nspid.last() == Some(&pi.pid) {
            return Err("pi_not_in_child_pid_namespace".into());
        }
        let init = self.proc.view(pi.ppid).ok_or("namespace_init_unreadable")?;
        let host_ns = self.proc.self_pidns().ok_or("host_pidns_unreadable")?;
        if init.nspid.last() != Some(&1) || init.pidns != pi.pidns || pi.pidns == host_ns {
            return Err("pi_not_in_bwrap_pid_namespace".into());
        }
        let leader = self
            .proc
            .view(init.ppid)
            .ok_or("sandbox_leader_unreadable")?;
        if leader.nspid.len() != 1
            || leader.pidns != host_ns
            || leader.argv.first().map(String::as_str) != Some(BWRAP)
            || Some(leader.exe) != self.proc.bwrap_exe()
            || init.argv != leader.argv
        {
            return Err("sandbox_leader_not_host_bwrap".into());
        }
        if [leader.pgrp, init.pgrp, pi.pgrp]
            .iter()
            .any(|pgrp| *pgrp != leader.pid)
        {
            return Err("sandbox_group_mismatch".into());
        }
        let hash = policy_hash(&leader.argv);
        if hash != query.policy.policy_hash {
            return Err("policy_hash_mismatch".into());
        }
        if leader.birth < query.launch_floor_ticks
            || leader.birth > init.birth
            || init.birth > pi.birth
        {
            return Err("sandbox_birth_order_wrong".into());
        }
        if expected_environ(&leader.argv).as_ref() != Some(&pi.environ) {
            return Err("pi_environ_not_exact_policy".into());
        }
        Ok(EnforcementAttestation {
            policy_hash: hash,
            covered_from_birth: birth,
        })
    }
}

#[cfg(target_os = "linux")]
pub struct LinuxProc;

#[cfg(target_os = "linux")]
impl ProcSource for LinuxProc {
    fn view(&self, pid: u32) -> Option<ProcView> {
        use std::os::unix::fs::MetadataExt as _;
        let base = std::path::PathBuf::from(format!("/proc/{pid}"));
        let status = std::fs::read_to_string(base.join("status")).ok()?;
        let stat = std::fs::read(base.join("stat")).ok()?;
        let close = stat.windows(2).rposition(|window| window == b") ")?;
        let fields: Vec<&[u8]> = stat[close + 2..]
            .split(|byte| *byte == b' ')
            .filter(|field| !field.is_empty())
            .collect();
        let number = |index: usize| -> Option<u64> {
            std::str::from_utf8(fields.get(index)?)
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        let nspid = status
            .lines()
            .find_map(|line| line.strip_prefix("NSpid:"))?
            .split_whitespace()
            .map(|value| value.parse().ok())
            .collect::<Option<Vec<u32>>>()?;
        let exe = std::fs::metadata(base.join("exe")).ok()?;
        let environ = std::fs::read(base.join("environ"))
            .ok()?
            .split(|byte| *byte == 0)
            .filter(|record| !record.is_empty())
            .map(|record| {
                let text = std::str::from_utf8(record).ok()?;
                let (key, value) = text.split_once('=')?;
                Some((key.to_owned(), value.to_owned()))
            })
            .collect::<Option<BTreeMap<_, _>>>()?;
        Some(ProcView {
            pid,
            birth: number(19)?,
            ppid: u32::try_from(number(1)?).ok()?,
            pgrp: u32::try_from(number(2)?).ok()?,
            nspid,
            pidns: std::fs::read_link(base.join("ns/pid"))
                .ok()?
                .display()
                .to_string(),
            argv: std::fs::read(base.join("cmdline"))
                .ok()?
                .split(|byte| *byte == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8(arg.to_vec()).ok())
                .collect::<Option<Vec<_>>>()?,
            environ,
            exe: (exe.dev(), exe.ino()),
        })
    }

    fn self_pidns(&self) -> Option<String> {
        std::fs::read_link("/proc/self/ns/pid")
            .ok()
            .map(|path| path.display().to_string())
    }

    fn bwrap_exe(&self) -> Option<(u64, u64)> {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(BWRAP)
            .ok()
            .map(|meta| (meta.dev(), meta.ino()))
    }
}

/// The verifier the server uses when the feature is on.
pub fn production_verifier() -> std::sync::Arc<dyn EnforcementVerifier> {
    #[cfg(target_os = "linux")]
    {
        std::sync::Arc::new(KernelEnforcementVerifier { proc: LinuxProc })
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::sync::Arc::new(crate::child_report_closure::UnprovenEnforcementVerifier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_root(label: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "herdr-covered-launch-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        for dir in ["cwd", "home", "agent", "lock", "ro"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::canonicalize(root).unwrap()
    }

    fn params(root: &Path) -> CoveredLaunchParams {
        let at = |dir: &str| root.join(dir).display().to_string();
        CoveredLaunchParams {
            root: root.display().to_string(),
            cwd: at("cwd"),
            command: vec!["/usr/bin/node-22".into(), at("lock/cli.js")],
            lock_dirs: vec![at("lock")],
            writable: vec![at("cwd"), at("home"), at("agent")],
            readonly: vec![at("ro")],
        }
    }

    #[test]
    fn policy_argv_matches_the_pinned_145_shape_with_two_herdr_names() {
        let root = private_root("argv");
        let env = child_env(
            &root.display().to_string(),
            [
                (
                    "HERDR_MAILBOX_BOOTSTRAP_ADDRESS".into(),
                    "/run/b.sock".into(),
                ),
                ("HERDR_AGENT".into(), "pi".into()),
                ("HERDR_PANE_ID".into(), "w1:p1".into()),
                ("HERDR_CLIENT_SOCKET_PATH".into(), "/run/h.sock".into()),
            ],
        )
        .unwrap();
        assert!(env.keys().filter(|key| key.starts_with("HERDR_")).eq([
            "HERDR_AGENT",
            "HERDR_MAILBOX_BOOTSTRAP_ADDRESS"
        ]
        .iter()));
        let args = vec!["--session".to_string(), "/s.jsonl".to_string()];
        let argv = bwrap_argv(&params(&root), &[], &env, &args, 1000, 1000).unwrap();
        assert_eq!(
            &argv[..7],
            [BWRAP]
                .iter()
                .chain(&NAMESPACE_FLAGS)
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .as_slice()
        );
        let tail = argv.iter().position(|arg| arg == "--").unwrap();
        assert_eq!(
            &argv[tail + 1..],
            [
                "/usr/bin/node-22".to_string(),
                root.join("lock/cli.js").display().to_string(),
                "--session".into(),
                "/s.jsonl".into()
            ]
        );
        let order: Vec<_> = argv
            .iter()
            .enumerate()
            .filter(|(_, arg)| *arg == "--bind" || *arg == "--ro-bind")
            .map(|(index, _)| argv[index + 1].clone())
            .collect();
        assert_eq!(
            order,
            ["/usr", "/etc"]
                .iter()
                .map(|s| s.to_string())
                .chain(
                    ["cwd", "home", "agent", "ro", "lock"]
                        .iter()
                        .map(|dir| root.join(dir).display().to_string())
                )
                .collect::<Vec<_>>()
        );
        assert_eq!(
            expected_environ(&argv).unwrap(),
            env.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .chain([("PWD".to_string(), root.join("cwd").display().to_string())])
                .collect()
        );
        assert_eq!(policy_hash(&argv), policy_hash(&argv.clone()));
        let mut changed = argv.clone();
        changed.push("--x".into());
        assert_ne!(policy_hash(&argv), policy_hash(&changed));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn policy_rejects_unsafe_binds_env_and_commands() {
        let root = private_root("reject");
        let env = child_env(&root.display().to_string(), []).unwrap();
        let base = params(&root);
        let reject = |params: CoveredLaunchParams| {
            bwrap_argv(&params, &[], &env, &[], 1000, 1000)
                .unwrap_err()
                .0
        };
        let mut shadow = base.clone();
        shadow.writable.push(root.display().to_string());
        assert_eq!(reject(shadow), "writable_bind_would_shadow_readonly");
        let mut outside = base.clone();
        outside.readonly.push("/usr/share".into());
        assert_eq!(reject(outside), "bind_outside_candidate_root");
        let mut relative = base.clone();
        relative.cwd = format!("{}/cwd/../cwd", root.display());
        assert_eq!(reject(relative), "policy_path_not_canonical_absolute");
        let mut cwd = base.clone();
        cwd.cwd = root.join("ro").display().to_string();
        assert_eq!(reject(cwd), "cwd_not_writable_bind");
        let mut command = base.clone();
        command.command = vec!["node".into()];
        assert_eq!(reject(command), "command_not_absolute");
        let mut forbidden = env.clone();
        forbidden.insert("NODE_OPTIONS".into(), "--inspect".into());
        assert_eq!(
            bwrap_argv(&base, &[], &forbidden, &[], 1000, 1000)
                .unwrap_err()
                .0,
            "forbidden_env_present"
        );
        let mut missing = env.clone();
        missing.remove("JITI_FS_CACHE");
        assert_eq!(
            verify_env(&missing).unwrap_err().0,
            "required_cache_env_missing"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    struct FakeProc(BTreeMap<u32, ProcView>);

    impl ProcSource for FakeProc {
        fn view(&self, pid: u32) -> Option<ProcView> {
            self.0.get(&pid).cloned()
        }
        fn self_pidns(&self) -> Option<String> {
            Some("pid:[1]".into())
        }
        fn bwrap_exe(&self) -> Option<(u64, u64)> {
            Some((1, 7))
        }
    }

    fn sandbox(argv: &[String]) -> BTreeMap<u32, ProcView> {
        let environ = expected_environ(argv).unwrap();
        let view = |pid, birth, ppid, nspid: Vec<u32>, pidns: &str, exe| ProcView {
            pid,
            birth,
            ppid,
            pgrp: 100,
            nspid,
            pidns: pidns.into(),
            argv: argv.to_vec(),
            environ: environ.clone(),
            exe,
        };
        let mut pi = view(102, 520, 101, vec![102, 2], "pid:[9]", (3, 3));
        pi.argv = vec!["pi".into()];
        [
            (100, view(100, 500, 1, vec![100], "pid:[1]", (1, 7))),
            (101, view(101, 510, 100, vec![101, 1], "pid:[9]", (1, 7))),
            (102, pi),
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn kernel_verifier_accepts_only_the_exact_sandboxed_launch() {
        let root = private_root("verify");
        let env = child_env(&root.display().to_string(), []).unwrap();
        let argv = bwrap_argv(&params(&root), &[], &env, &[], 1000, 1000).unwrap();
        let birth = BirthRecord {
            pid: 102,
            start_ticks: 520,
        };
        let policy = CoveredLaunchPolicy {
            policy_id: POLICY_ID.into(),
            policy_hash: policy_hash(&argv),
            receipt_digest: "d".repeat(64),
            sandboxed_birth: birth,
        };
        let query = EnforcementQuery {
            policy: &policy,
            managed_launch_birth: birth,
            launch_floor_ticks: 450,
        };
        let verify = |procs: BTreeMap<u32, ProcView>, query: &EnforcementQuery<'_>| {
            KernelEnforcementVerifier {
                proc: FakeProc(procs),
            }
            .verify(query)
        };
        let attested = verify(sandbox(&argv), &query).unwrap();
        assert_eq!(attested.covered_from_birth, birth);
        assert!(crate::child_report_closure::enforcement_covers_launch(
            &query, &attested
        ));
        type Mutation = Box<dyn Fn(&mut BTreeMap<u32, ProcView>)>;
        let cases: Vec<(&str, Mutation)> = vec![
            (
                "launch_birth_mismatch",
                Box::new(|p| p.get_mut(&102).unwrap().birth = 521),
            ),
            (
                "pi_not_in_child_pid_namespace",
                Box::new(|p| p.get_mut(&102).unwrap().nspid = vec![102]),
            ),
            (
                "pi_not_in_bwrap_pid_namespace",
                Box::new(|p| p.get_mut(&102).unwrap().pidns = "pid:[1]".into()),
            ),
            (
                "pi_not_in_bwrap_pid_namespace",
                Box::new(|p| p.get_mut(&101).unwrap().nspid = vec![101, 2]),
            ),
            (
                "sandbox_leader_not_host_bwrap",
                Box::new(|p| p.get_mut(&100).unwrap().exe = (1, 8)),
            ),
            (
                "sandbox_leader_not_host_bwrap",
                Box::new(|p| p.get_mut(&100).unwrap().argv[0] = "/tmp/bwrap".into()),
            ),
            (
                "sandbox_group_mismatch",
                Box::new(|p| p.get_mut(&102).unwrap().pgrp = 102),
            ),
            (
                "sandbox_birth_order_wrong",
                Box::new(|p| p.get_mut(&100).unwrap().birth = 400),
            ),
            (
                "sandbox_birth_order_wrong",
                Box::new(|p| p.get_mut(&101).unwrap().birth = 530),
            ),
            (
                "pi_environ_not_exact_policy",
                Box::new(|p| {
                    p.get_mut(&102)
                        .unwrap()
                        .environ
                        .insert("NODE_OPTIONS".into(), "x".into());
                }),
            ),
        ];
        for (expected, mutate) in cases {
            let mut procs = sandbox(&argv);
            mutate(&mut procs);
            assert_eq!(verify(procs, &query).unwrap_err(), expected);
        }
        let other = CoveredLaunchPolicy {
            policy_hash: "e".repeat(64),
            ..policy.clone()
        };
        let wrong_hash = EnforcementQuery {
            policy: &other,
            ..query.clone()
        };
        assert_eq!(
            verify(sandbox(&argv), &wrong_hash).unwrap_err(),
            "policy_hash_mismatch"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// Disposable local proof on a real kernel (run explicitly with
    /// `HERDR_COVERED_LAUNCH_PROOF_ROOT=<fresh private dir> -- --ignored`):
    /// a real bwrap tree built from this policy passes the kernel verifier,
    /// while the same process under the wrong policy hash, a wrong birth, or
    /// an unsandboxed process fails.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "spawns /usr/bin/bwrap; run explicitly as a disposable local proof"]
    fn real_bwrap_launch_passes_the_kernel_verifier() {
        use std::os::unix::process::CommandExt as _;
        let base = std::path::PathBuf::from(
            std::env::var("HERDR_COVERED_LAUNCH_PROOF_ROOT")
                .expect("set HERDR_COVERED_LAUNCH_PROOF_ROOT to a fresh private directory"),
        );
        assert!(
            !base.exists() || std::fs::read_dir(&base).unwrap().next().is_none(),
            "proof root must be fresh"
        );
        for dir in ["cwd", "home", "agent"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        let root = std::fs::canonicalize(&base).unwrap();
        let at = |dir: &str| root.join(dir).display().to_string();
        let params = CoveredLaunchParams {
            root: root.display().to_string(),
            cwd: at("cwd"),
            command: vec!["/usr/bin/sleep".into()],
            lock_dirs: vec![],
            writable: vec![at("cwd"), at("home"), at("agent")],
            readonly: vec![],
        };
        let env = child_env(
            &params.root,
            [("HERDR_AGENT".to_string(), "pi".to_string())],
        )
        .unwrap();
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let floor = crate::platform::first_post_launch_birth_tick().unwrap();
        assert!(crate::platform::wait_until_birth_tick(floor));
        let argv = bwrap_argv(&params, &[], &env, &["30".into()], uid, gid).unwrap();
        struct Kill(std::process::Child);
        impl Drop for Kill {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let leader = Kill(
            std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .env_clear()
                .env("PATH", "/usr/bin")
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let leader_pid = leader.0.id();
        let proc = LinuxProc;
        let mut sleeper = None;
        for _ in 0..200 {
            let children =
                std::fs::read_to_string(format!("/proc/{leader_pid}/task/{leader_pid}/children"))
                    .unwrap_or_default();
            if let Some(init) = children
                .split_whitespace()
                .next()
                .and_then(|pid| pid.parse::<u32>().ok())
            {
                let grand = std::fs::read_to_string(format!("/proc/{init}/task/{init}/children"))
                    .unwrap_or_default();
                if let Some(pid) = grand.split_whitespace().next().and_then(|p| p.parse().ok()) {
                    if proc.view(pid).is_some_and(|view| {
                        view.argv.first().map(String::as_str) == Some("/usr/bin/sleep")
                    }) {
                        sleeper = Some(pid);
                        break;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let pid = sleeper.expect("sandboxed command started");
        let birth: BirthRecord = crate::platform::process_birth_identity(pid).unwrap().into();
        let policy = CoveredLaunchPolicy {
            policy_id: POLICY_ID.into(),
            policy_hash: policy_hash(&argv),
            receipt_digest: "d".repeat(64),
            sandboxed_birth: birth,
        };
        let query = EnforcementQuery {
            policy: &policy,
            managed_launch_birth: birth,
            launch_floor_ticks: floor,
        };
        let verifier = KernelEnforcementVerifier { proc: LinuxProc };
        let attested = verifier.verify(&query).expect("real sandbox verifies");
        assert!(crate::child_report_closure::enforcement_covers_launch(
            &query, &attested
        ));
        let wrong = CoveredLaunchPolicy {
            policy_hash: "e".repeat(64),
            ..policy.clone()
        };
        assert_eq!(
            verifier
                .verify(&EnforcementQuery {
                    policy: &wrong,
                    ..query.clone()
                })
                .unwrap_err(),
            "policy_hash_mismatch"
        );
        let shifted = BirthRecord {
            start_ticks: birth.start_ticks + 1,
            ..birth
        };
        assert_eq!(
            verifier
                .verify(&EnforcementQuery {
                    managed_launch_birth: shifted,
                    ..query.clone()
                })
                .unwrap_err(),
            "launch_birth_mismatch"
        );
        let plain = Kill(
            std::process::Command::new("/usr/bin/sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let plain_birth: BirthRecord = crate::platform::process_birth_identity(plain.0.id())
            .unwrap()
            .into();
        assert_eq!(
            verifier
                .verify(&EnforcementQuery {
                    managed_launch_birth: plain_birth,
                    ..query.clone()
                })
                .unwrap_err(),
            "pi_not_in_child_pid_namespace"
        );
        drop(plain);
        drop(leader);
        eprintln!(
            "covered-launch proof: leader={leader_pid} pid={pid} birth={} hash={}",
            birth.start_ticks, policy.policy_hash
        );
    }
}
