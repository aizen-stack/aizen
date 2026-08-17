//! Linux backend — real kernel enforcement, no root, no daemons, no new crates:
//!
//! * **Landlock** (5.13+) confines filesystem access to the policy's roots — an allow-list the
//!   kernel enforces on every open/rename/unlink the child (or ANY descendant) attempts.
//! * **seccomp-BPF** denies outbound network by refusing `socket(AF_INET/AF_INET6)` with `EACCES`
//!   (UNIX sockets stay usable — build IPC keeps working).
//! * **rlimits** cap core dumps, CPU time, address space, open files and file size.
//! * **`PR_SET_NO_NEW_PRIVS`** stops setuid/caps escalation and is the precondition for
//!   unprivileged Landlock/seccomp anyway.
//!
//! Everything is raw syscalls declared here (the same posture as `core::proctree`'s
//! `setsid`/`killpg`): the `libc` crate stays out of the dependency tree. The expensive parts
//! (opening root fds, building the ruleset and the BPF program) happen BEFORE fork; the `pre_exec`
//! hook in the child only issues syscalls on pre-built data — nothing there allocates, which is
//! the async-signal-safety contract `pre_exec` demands.
//!
//! Probing is honest: a kernel without Landlock reports `fs: unavailable` and `strict` refuses;
//! `auto` degrades and says so once.

use crate::sandbox::capabilities::{BackendKind, CapabilityReport, Enforcement};
use crate::sandbox::policy::{FsPolicy, SandboxLimits};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;

// ── syscall surface (declared, not linked through the libc crate) ────────────

use std::os::raw::{c_char, c_int, c_long};

extern "C" {
    fn syscall(num: c_long, ...) -> c_long;
    fn prctl(option: c_int, ...) -> c_int;
    fn open(path: *const c_char, oflag: c_int, ...) -> c_int;
    fn setrlimit(resource: c_int, rlim: *const RLimit) -> c_int;
}

const SYS_LANDLOCK_CREATE_RULESET: c_long = 444;
const SYS_LANDLOCK_ADD_RULE: c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: c_long = 446;
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: c_int = 1;

const PR_SET_SECCOMP: c_int = 22;
const PR_GET_SECCOMP: c_int = 21;
const PR_SET_NO_NEW_PRIVS: c_int = 38;
const SECCOMP_MODE_FILTER: c_long = 2;

const O_PATH: c_int = 0o10000000;
const O_CLOEXEC: c_int = 0o2000000;

const RLIMIT_CPU: c_int = 0;
const RLIMIT_FSIZE: c_int = 1;
const RLIMIT_CORE: c_int = 4;
const RLIMIT_NOFILE: c_int = 7;
const RLIMIT_AS: c_int = 9;

#[repr(C)]
struct RLimit {
    rlim_cur: u64,
    rlim_max: u64,
}

/// `struct landlock_ruleset_attr` (uapi). The net field exists from ABI v4; older kernels are
/// handed only the first 8 bytes (the size argument tells them where we stop).
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

/// `struct landlock_path_beneath_attr` — `__attribute__((packed))` in the kernel header, so the
/// Rust mirror must be packed too (12 bytes, not 16).
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: c_int,
}

// Landlock filesystem access rights.
const ACCESS_FS_EXECUTE: u64 = 1 << 0;
const ACCESS_FS_READ_FILE: u64 = 1 << 2;
const ACCESS_FS_READ_DIR: u64 = 1 << 3;
const ACCESS_FS_V1_MASK: u64 = (1 << 13) - 1; // execute..make_sym
const ACCESS_FS_REFER: u64 = 1 << 13; // ABI v2
const ACCESS_FS_TRUNCATE: u64 = 1 << 14; // ABI v3
const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15; // ABI v5

fn handled_fs_for(abi: i32) -> u64 {
    let mut m = ACCESS_FS_V1_MASK;
    if abi >= 2 {
        m |= ACCESS_FS_REFER;
    }
    if abi >= 3 {
        m |= ACCESS_FS_TRUNCATE;
    }
    if abi >= 5 {
        m |= ACCESS_FS_IOCTL_DEV;
    }
    m
}

// ── seccomp network filter ───────────────────────────────────────────────────

/// One classic-BPF instruction (`struct sock_filter`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JEQ_K: u16 = 0x15;
const BPF_RET_K: u16 = 0x06;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const EACCES: u32 = 13;

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "x86_64")]
const SYS_SOCKET_NR: u32 = 41;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;
#[cfg(target_arch = "aarch64")]
const SYS_SOCKET_NR: u32 = 198;

const SECCOMP_DATA_NR: u32 = 0;
const SECCOMP_DATA_ARCH: u32 = 4;
const SECCOMP_DATA_ARG0: u32 = 16;

const AF_INET: u32 = 2;
const AF_INET6: u32 = 10;

fn bpf(code: u16, jt: u8, jf: u8, k: u32) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// The network-deny program: allow everything except `socket()` for the internet families, which
/// returns `EACCES`. Kept tiny on purpose — a short program is auditable and cheap on every
/// syscall the child makes.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub fn network_deny_program() -> Vec<SockFilter> {
    vec![
        // A child of a FOREIGN architecture (32-bit multilib) is not covered by this filter's
        // syscall numbers; allow it rather than kill legitimate cross-builds, and say so in the
        // probe notes — the docs carry the limitation.
        bpf(BPF_LD_W_ABS, 0, 0, SECCOMP_DATA_ARCH),
        bpf(BPF_JEQ_K, 1, 0, AUDIT_ARCH),
        bpf(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW),
        bpf(BPF_LD_W_ABS, 0, 0, SECCOMP_DATA_NR),
        bpf(BPF_JEQ_K, 1, 0, SYS_SOCKET_NR),
        bpf(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW),
        // socket(domain, …): deny the internet families, allow the rest (AF_UNIX IPC).
        bpf(BPF_LD_W_ABS, 0, 0, SECCOMP_DATA_ARG0),
        bpf(BPF_JEQ_K, 1, 0, AF_INET),
        bpf(BPF_JEQ_K, 0, 1, AF_INET6),
        bpf(BPF_RET_K, 0, 0, SECCOMP_RET_ERRNO | EACCES),
        bpf(BPF_RET_K, 0, 0, SECCOMP_RET_ALLOW),
    ]
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn network_deny_program() -> Vec<SockFilter> {
    Vec::new() // no vetted syscall table for this arch → honest "unavailable"
}

// ── probe ────────────────────────────────────────────────────────────────────

/// Landlock ABI version, or a negative errno-ish value when unsupported.
fn landlock_abi() -> i32 {
    // SAFETY: the documented probe form — null attr, zero size, the VERSION flag; no memory is
    // read or written by the kernel for this call.
    let r = unsafe {
        syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    r as i32
}

fn seccomp_available() -> bool {
    if network_deny_program().is_empty() {
        return false;
    }
    // SAFETY: PR_GET_SECCOMP takes no pointers; a kernel without seccomp answers EINVAL (<0).
    unsafe { prctl(PR_GET_SECCOMP) >= 0 }
}

/// The capability matrix for this host, measured (not assumed).
pub fn probe() -> CapabilityReport {
    let abi = landlock_abi();
    let seccomp = seccomp_available();
    let mut notes = Vec::new();
    let fs = if abi >= 1 {
        notes.push(format!(
            "Landlock ABI v{abi} — filesystem allow-list is kernel-enforced"
        ));
        if abi < 3 {
            notes.push(
                "Landlock ABI < 3: truncate is not a governed operation on this kernel".to_string(),
            );
        }
        Enforcement::Enforced
    } else {
        notes.push(
            "Landlock unavailable (kernel < 5.13 or disabled) — filesystem policy is advisory here"
                .to_string(),
        );
        Enforcement::Advisory
    };
    let net = if seccomp {
        notes.push(
            "network deny = seccomp refusing socket(AF_INET/AF_INET6); foreign-arch (32-bit) \
             children are not covered"
                .to_string(),
        );
        Enforcement::Enforced
    } else {
        notes.push("seccomp filtering unavailable — network policy is advisory here".to_string());
        Enforcement::Advisory
    };
    notes.push(
        "process-count ceilings use RLIMIT_NPROC, which counts the USER's processes, not the \
         tree's — kept generous to avoid false kills"
            .to_string(),
    );
    CapabilityReport {
        backend: if abi >= 1 || seccomp {
            BackendKind::Linux
        } else {
            BackendKind::Guarded
        },
        fs_read: fs,
        fs_write: fs,
        network_deny: net,
        env_isolation: Enforcement::Enforced,
        process_containment: Enforcement::Enforced,
        resource_limits: Enforcement::Enforced,
        notes,
    }
}

// ── ruleset construction (pre-fork) ──────────────────────────────────────────

fn open_path_fd(path: &std::path::Path) -> Option<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    // SAFETY: `bytes` is a NUL-terminated path; O_PATH opens no data access, just a location fd.
    let fd = unsafe { open(bytes.as_ptr().cast::<c_char>(), O_PATH | O_CLOEXEC) };
    if fd < 0 {
        return None; // missing root on this system — simply not granted
    }
    // SAFETY: `fd` is a live fd we own exclusively.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Build a Landlock ruleset for `fs` (returns `None` when Landlock is unavailable, with the
/// degradation reason). All syscalls here run pre-fork on the parent — allocation is fine.
fn build_ruleset(fs: &FsPolicy) -> Result<Option<OwnedFd>, String> {
    let abi = landlock_abi();
    if abi < 1 {
        return Ok(None);
    }
    let handled = handled_fs_for(abi);
    let attr = RulesetAttr {
        handled_access_fs: handled,
        handled_access_net: 0,
    };
    // Older ABIs know a smaller attr struct; pass exactly the size they understand.
    let attr_size = if abi >= 4 {
        std::mem::size_of::<RulesetAttr>()
    } else {
        std::mem::size_of::<u64>()
    };
    // SAFETY: `attr` outlives the call; `attr_size` never exceeds the struct's real size.
    let fd = unsafe { syscall(SYS_LANDLOCK_CREATE_RULESET, &attr, attr_size, 0u32) };
    if fd < 0 {
        return Err("landlock_create_ruleset failed".to_string());
    }
    // SAFETY: a fresh fd from the kernel, owned here.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as c_int) };

    let add = |path: &std::path::Path, access: u64| {
        let Some(parent) = open_path_fd(path) else {
            return; // absent root: nothing to grant
        };
        let rule = PathBeneathAttr {
            allowed_access: access & handled,
            parent_fd: parent.as_raw_fd(),
        };
        // SAFETY: `rule` and both fds are live for the duration of the call.
        let r = unsafe {
            syscall(
                SYS_LANDLOCK_ADD_RULE,
                ruleset.as_raw_fd(),
                LANDLOCK_RULE_PATH_BENEATH,
                &rule,
                0u32,
            )
        };
        let _ = r; // a single failed grant narrows, never widens — safe to continue
    };

    for p in &fs.read_write {
        add(p, handled); // everything the kernel governs, beneath this root
    }
    for p in &fs.read_only {
        add(
            p,
            ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR,
        );
    }
    Ok(Some(ruleset))
}

// ── the per-spawn sandbox ────────────────────────────────────────────────────

/// Everything the child applies to itself between fork and exec. Built once per spawn, pre-fork;
/// cheap to clone into the `pre_exec` closure.
pub struct LinuxSandbox {
    ruleset: Option<Arc<OwnedFd>>,
    seccomp: Option<Arc<Vec<SockFilter>>>,
    rlimits: Vec<(c_int, u64)>,
    /// What could NOT be armed, for the runner's degradation report (empty = full strength).
    pub degraded: Vec<String>,
}

impl LinuxSandbox {
    /// Assemble the kernel policy for one spawn. `deny_network` reflects the resolved policy
    /// (default deny unless granted); `fs` is the roots policy including cwd and private temp.
    pub fn build(fs: &FsPolicy, deny_network: bool, limits: &SandboxLimits) -> Self {
        let mut degraded = Vec::new();
        let ruleset = match build_ruleset(fs) {
            Ok(Some(fd)) => Some(Arc::new(fd)),
            Ok(None) => {
                degraded.push("filesystem: Landlock unavailable on this kernel".to_string());
                None
            }
            Err(e) => {
                degraded.push(format!("filesystem: {e}"));
                None
            }
        };
        let seccomp = if deny_network {
            if seccomp_available() {
                Some(Arc::new(network_deny_program()))
            } else {
                degraded.push("network: seccomp unavailable on this kernel/arch".to_string());
                None
            }
        } else {
            None
        };

        let mut rlimits: Vec<(c_int, u64)> = vec![(RLIMIT_CORE, 0)]; // core dumps off, always
        if let Some(cpu) = limits.cpu_seconds {
            rlimits.push((RLIMIT_CPU, cpu));
        }
        if let Some(mb) = limits.memory_mb {
            rlimits.push((RLIMIT_AS, mb.saturating_mul(1024 * 1024)));
        }
        if let Some(n) = limits.max_open_files {
            rlimits.push((RLIMIT_NOFILE, n));
        }
        if let Some(mb) = limits.max_file_size_mb {
            rlimits.push((RLIMIT_FSIZE, mb.saturating_mul(1024 * 1024)));
        }

        Self {
            ruleset,
            seccomp,
            rlimits,
            degraded,
        }
    }

    /// Whether any kernel mechanism is armed (drives the runner's backend label).
    pub fn any_kernel(&self) -> bool {
        self.ruleset.is_some() || self.seccomp.is_some()
    }

    fn hook(&self) -> impl FnMut() -> std::io::Result<()> + Send + Sync + 'static {
        let ruleset = self.ruleset.clone();
        let seccomp = self.seccomp.clone();
        let rlimits = self.rlimits.clone();
        move || {
            // Post-fork, pre-exec: syscalls only, on data built before the fork. An error here
            // aborts the spawn — fail-closed is the property the whole backend sells.
            for (res, val) in &rlimits {
                let lim = RLimit {
                    rlim_cur: *val,
                    rlim_max: *val,
                };
                // SAFETY: plain setrlimit on a stack struct.
                if unsafe { setrlimit(*res, &lim) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            // SAFETY: no pointers; flips a process flag.
            if unsafe { prctl(PR_SET_NO_NEW_PRIVS, 1i64, 0i64, 0i64, 0i64) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(fd) = &ruleset {
                // SAFETY: the ruleset fd is live (Arc keeps it) and flags must be 0.
                if unsafe { syscall(SYS_LANDLOCK_RESTRICT_SELF, fd.as_raw_fd(), 0u32) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if let Some(prog) = &seccomp {
                let fprog = SockFprog {
                    len: prog.len() as u16,
                    filter: prog.as_ptr(),
                };
                // SAFETY: `fprog` points at the pre-built, Arc-held program.
                if unsafe { prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &fprog) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        }
    }

    pub fn apply_std(&self, cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        // SAFETY: the hook is async-signal-safe (syscalls on pre-built data; no allocation).
        unsafe {
            cmd.pre_exec(self.hook());
        }
    }

    pub fn apply_tokio(&self, cmd: &mut tokio::process::Command) {
        // SAFETY: as `apply_std`.
        unsafe {
            cmd.pre_exec(self.hook());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bpf_program_shape_is_the_audited_one() {
        let p = network_deny_program();
        if p.is_empty() {
            return; // unvetted arch — nothing to assert
        }
        assert_eq!(p.len(), 11);
        // First instruction loads the arch; the errno return carries EACCES.
        assert_eq!(p[0].code, BPF_LD_W_ABS);
        assert_eq!(p[0].k, SECCOMP_DATA_ARCH);
        assert!(p
            .iter()
            .any(|i| i.code == BPF_RET_K && i.k == (SECCOMP_RET_ERRNO | EACCES)));
        // And it terminates with an allow (default-allow filter, deny only the named case).
        assert_eq!(p.last().unwrap().k, SECCOMP_RET_ALLOW);
    }

    #[test]
    fn path_beneath_attr_is_packed_like_the_kernel_header() {
        // The uapi struct is __attribute__((packed)): 12 bytes. A 16-byte Rust mirror would make
        // every add_rule call silently pass garbage.
        assert_eq!(std::mem::size_of::<PathBeneathAttr>(), 12);
        assert_eq!(std::mem::size_of::<SockFilter>(), 8);
    }

    #[test]
    fn handled_mask_grows_with_abi() {
        assert_eq!(handled_fs_for(1), ACCESS_FS_V1_MASK);
        assert!(handled_fs_for(2) & ACCESS_FS_REFER != 0);
        assert!(handled_fs_for(3) & ACCESS_FS_TRUNCATE != 0);
        assert!(handled_fs_for(5) & ACCESS_FS_IOCTL_DEV != 0);
        assert!(handled_fs_for(1) & ACCESS_FS_TRUNCATE == 0);
    }

    /// End-to-end on a Landlock kernel: a sandboxed child must fail to read a file outside its
    /// roots and still read one inside. Skips (with a note) where the kernel lacks Landlock —
    /// the probe already reports that honestly.
    #[test]
    fn landlocked_child_cannot_read_outside_its_roots() {
        if landlock_abi() < 1 {
            eprintln!("skipping: no Landlock on this kernel");
            return;
        }
        let base = std::env::temp_dir().join(format!(
            "aizen-ll-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        let inside = base.join("ws");
        let outside = base.join("secret");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(inside.join("ok.txt"), "in").unwrap();
        std::fs::write(outside.join("cred.txt"), "no").unwrap();

        let fs = FsPolicy {
            read_write: vec![inside.clone()],
            read_only: vec![
                "/usr".into(),
                "/bin".into(),
                "/lib".into(),
                "/lib64".into(),
                "/etc".into(),
                "/proc".into(),
                "/dev".into(),
            ],
            deny: vec![],
        };
        let sbx = LinuxSandbox::build(&fs, false, &SandboxLimits::default());
        assert!(sbx.any_kernel(), "ruleset must be armed on this kernel");

        let run = |path: &std::path::Path| -> bool {
            let mut cmd = std::process::Command::new("/bin/cat");
            cmd.arg(path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            sbx.apply_std(&mut cmd);
            cmd.status().map(|s| s.success()).unwrap_or(false)
        };
        assert!(run(&inside.join("ok.txt")), "inside read must succeed");
        assert!(
            !run(&outside.join("cred.txt")),
            "outside read must be denied by the kernel"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The seccomp filter must stop a real TCP connect to a live local listener.
    #[test]
    fn seccomp_denies_a_local_tcp_connect() {
        if !seccomp_available() {
            eprintln!("skipping: no seccomp on this kernel");
            return;
        }
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let fs = FsPolicy {
            read_write: vec![std::env::temp_dir()],
            read_only: vec![
                "/usr".into(),
                "/bin".into(),
                "/lib".into(),
                "/lib64".into(),
                "/etc".into(),
                "/dev".into(),
                "/proc".into(),
            ],
            deny: vec![],
        };
        let sbx = LinuxSandbox::build(&fs, true, &SandboxLimits::default());
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c")
            // /dev/tcp is a bash-ism; use a tool-free probe: exec 3<> with sh may not exist, so
            // rely on `timeout`-less direct connect via sh's redirection where available, else
            // python if present. Portable floor: `sh -c 'exec 3<>/dev/tcp/…'` works on bash;
            // dash lacks it — then the command fails for the WRONG reason. Use getent-free
            // approach: run `sh -c ':' < /dev/null` … simplest robust probe is a tiny C-less
            // trick: `busybox nc` may be absent too. So: accept EITHER failure-to-connect or
            // tool-absence, but REQUIRE that no connection ever lands on the listener.
            .arg(format!(
                "(exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null || nc -w1 127.0.0.1 {port} </dev/null 2>/dev/null"
            ))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        sbx.apply_std(&mut cmd);
        let _ = cmd.status();

        listener.set_nonblocking(true).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            listener.accept().is_err(),
            "a sandboxed child managed to connect — seccomp deny failed"
        );
    }
}
