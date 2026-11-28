//! Linux `seccomp-unotify` process-scoped write observer.
//! Intercepts mutating file syscalls (`openat`, `openat2`, `unlinkat`,
//! `renameat`, `renameat2`, `truncate`, plus legacy x86_64 equivalents),
//! lazily snapshots pre-write baselines in the host supervisor, and resumes
//! the child syscall unmodified via `SECCOMP_USER_NOTIF_FLAG_CONTINUE`.

use std::ffi::CString;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::io::Read;
use std::io::Write;
use std::mem;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;

use crate::CODEX_EXEC_OBSERVE_ARG1;
use crate::WrappedCommand;
use crate::persist::ObservationState;

const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;
const SECCOMP_FILTER_FLAG_NEW_LISTENER: libc::c_ulong = 1 << 3;
const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_USER_NOTIF_FLAG_CONTINUE: u32 = 1;

const SECCOMP_IOCTL_NOTIF_RECV: libc::c_ulong = 0xc050_2100;
const SECCOMP_IOCTL_NOTIF_SEND: libc::c_ulong = 0xc018_2101;
const SECCOMP_IOCTL_NOTIF_ID_VALID: libc::c_ulong = 0x4008_2102;

const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_JSET: u16 = 0x40;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
const OFFSET_ARG1_LO: u32 = 24;
const OFFSET_ARG2_LO: u32 = 32;

const WRITE_OPEN_FLAGS: u32 =
    (libc::O_WRONLY | libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC) as u32;

#[cfg(target_arch = "x86_64")]
const EXPECTED_AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const EXPECTED_AUDIT_ARCH: u32 = 0xc000_00b7;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SeccompData {
    nr: i32,
    arch: u32,
    instruction_pointer: u64,
    args: [u64; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SeccompNotif {
    id: u64,
    pid: u32,
    flags: u32,
    data: SeccompData,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SeccompNotifResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockFilter {
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

fn bpf_stmt(code: u16, k: u32) -> SockFilter {
    SockFilter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn bpf_jump(code: u16, k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// Build a minimal BPF program that traps only file-mutating syscalls.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn build_write_filter() -> Vec<SockFilter> {
    // Jump targets are computed relative to the two trailing RET instructions.
    let mut f = Vec::with_capacity(32);
    // 0: load arch; if arch != EXPECTED_AUDIT_ARCH -> allow
    f.push(bpf_stmt(BPF_LD | BPF_W | BPF_ABS, OFFSET_ARCH));
    // placeholder for arch mismatch jump -> filled below
    let arch_jump_idx = f.len();
    f.push(bpf_jump(
        BPF_JMP | BPF_JEQ | BPF_K,
        EXPECTED_AUDIT_ARCH,
        0,
        0,
    ));

    // Load syscall nr
    f.push(bpf_stmt(BPF_LD | BPF_W | BPF_ABS, OFFSET_NR));

    let mut to_notify = Vec::new();
    let to_allow = [arch_jump_idx];

    // Unconditional notify syscalls
    let mut unconditional = vec![
        libc::SYS_openat2 as u32,
        libc::SYS_unlinkat as u32,
        libc::SYS_renameat as u32,
        libc::SYS_renameat2 as u32,
        libc::SYS_truncate as u32,
    ];
    #[cfg(target_arch = "x86_64")]
    {
        unconditional.push(libc::SYS_creat as u32);
        unconditional.push(libc::SYS_unlink as u32);
        unconditional.push(libc::SYS_rename as u32);
    }
    for nr in unconditional {
        to_notify.push(f.len());
        f.push(bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, nr, 0, 0));
    }

    // openat(dirfd, path, flags, mode): notify only when write flags are set
    f.push(bpf_jump(
        BPF_JMP | BPF_JEQ | BPF_K,
        libc::SYS_openat as u32,
        0,
        2,
    ));
    f.push(bpf_stmt(BPF_LD | BPF_W | BPF_ABS, OFFSET_ARG2_LO));
    to_notify.push(f.len());
    f.push(bpf_jump(BPF_JMP | BPF_JSET | BPF_K, WRITE_OPEN_FLAGS, 0, 0));

    // x86_64 legacy open(path, flags, mode): notify only when write flags are set
    #[cfg(target_arch = "x86_64")]
    {
        // Reload nr in case openat fell through with read-only flags.
        f.push(bpf_stmt(BPF_LD | BPF_W | BPF_ABS, OFFSET_NR));
        f.push(bpf_jump(
            BPF_JMP | BPF_JEQ | BPF_K,
            libc::SYS_open as u32,
            0,
            2,
        ));
        f.push(bpf_stmt(BPF_LD | BPF_W | BPF_ABS, OFFSET_ARG1_LO));
        to_notify.push(f.len());
        f.push(bpf_jump(BPF_JMP | BPF_JSET | BPF_K, WRITE_OPEN_FLAGS, 0, 0));
    }

    let allow_idx = f.len();
    f.push(bpf_stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
    let notify_idx = f.len();
    f.push(bpf_stmt(BPF_RET | BPF_K, SECCOMP_RET_USER_NOTIF));

    for idx in to_allow {
        f[idx].jf = (allow_idx - idx - 1) as u8;
    }
    for idx in to_notify {
        f[idx].jt = (notify_idx - idx - 1) as u8;
    }
    f
}

pub(crate) struct LinuxObserver {
    sock_path: PathBuf,
    settled: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    handle: Mutex<Option<thread::JoinHandle<()>>>,
}

impl LinuxObserver {
    pub(crate) fn start(dir: &Path, state: Arc<Mutex<ObservationState>>) -> io::Result<Self> {
        let sock_path = dir.join("observe.sock");
        let listener = UnixListener::bind(&sock_path)?;
        listener.set_nonblocking(true)?;
        let settled = Arc::new(AtomicBool::new(false));
        let settled_bg = Arc::clone(&settled);
        let connected = Arc::new(AtomicBool::new(false));
        let connected_bg = Arc::clone(&connected);
        let handle = thread::Builder::new()
            .name("mekugi-seccomp-observer".to_string())
            .spawn(move || supervisor_loop(listener, state, settled_bg, connected_bg))?;
        Ok(Self {
            sock_path,
            settled,
            connected,
            handle: Mutex::new(Some(handle)),
        })
    }

    pub(crate) fn wrap_command(
        &self,
        argv: &[String],
        self_exe: Option<&Path>,
    ) -> Option<WrappedCommand> {
        let exe = self_exe?;
        if argv.is_empty() {
            return None;
        }
        let mut wrapped = Vec::with_capacity(argv.len() + 4);
        wrapped.push(exe.to_string_lossy().into_owned());
        wrapped.push(CODEX_EXEC_OBSERVE_ARG1.to_string());
        wrapped.push(self.sock_path.to_string_lossy().into_owned());
        wrapped.push("--".to_string());
        wrapped.extend(argv.iter().cloned());
        Some(WrappedCommand {
            argv: wrapped,
            env: Vec::new(),
        })
    }

    /// Stop recording new baselines while keeping the notification listener alive
    /// in pass-through mode (`CONTINUE`) until all descendants exit (`POLLHUP`),
    /// so surviving background jobs never fail with `ENOSYS`.
    pub(crate) fn settle(&self) {
        self.settled.store(true, Ordering::Release);
        if !self.connected.load(Ordering::Acquire)
            && let Some(handle) = self
                .handle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        {
            let _ = handle.join();
        }
    }
}

fn supervisor_loop(
    listener: UnixListener,
    state: Arc<Mutex<ObservationState>>,
    settled: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
) {
    let (listener_fd, wrapper_pid) = loop {
        if settled.load(Ordering::Acquire) {
            return;
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                match recv_fd(&stream) {
                    Ok(fd) => {
                        connected.store(true, Ordering::Release);
                        let _ = stream.write_all(&[1u8]);
                        break (fd, peer_pid(&stream));
                    }
                    Err(_) => return,
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                let mut pfd = libc::pollfd {
                    fd: listener.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                unsafe { libc::poll(&mut pfd, 1, 25) };
            }
            Err(_) => return,
        }
    };

    serve_notifications(listener_fd.as_raw_fd(), wrapper_pid, &state, &settled);
}

fn peer_pid(stream: &UnixStream) -> Option<u32> {
    let mut cred: libc::ucred = unsafe { mem::zeroed() };
    let mut size = mem::size_of_val(&cred) as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut size,
        )
    };
    (rc == 0).then(|| u32::try_from(cred.pid).ok()).flatten()
}

fn serve_notifications(
    notif_fd: RawFd,
    wrapper_pid: Option<u32>,
    state: &Mutex<ObservationState>,
    settled: &AtomicBool,
) {
    loop {
        let mut pfd = libc::pollfd {
            fd: notif_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms = if settled.load(Ordering::Acquire) {
            250
        } else {
            25
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc == 0 {
            continue;
        }
        if (pfd.revents & libc::POLLHUP) != 0 && (pfd.revents & libc::POLLIN) == 0 {
            break;
        }
        if (pfd.revents & libc::POLLIN) == 0 {
            continue;
        }

        let mut req = SeccompNotif::default();
        let recv_rc = unsafe { libc::ioctl(notif_fd, SECCOMP_IOCTL_NOTIF_RECV, &mut req) };
        if recv_rc < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) || err.kind() == io::ErrorKind::Interrupted
            {
                continue;
            }
            break;
        }

        if !settled.load(Ordering::Acquire) {
            handle_notification(notif_fd, &req, wrapper_pid, state);
        }

        let resp = SeccompNotifResp {
            id: req.id,
            val: 0,
            error: 0,
            flags: SECCOMP_USER_NOTIF_FLAG_CONTINUE,
        };
        unsafe {
            libc::ioctl(notif_fd, SECCOMP_IOCTL_NOTIF_SEND, &resp);
        }
    }
}

fn handle_notification(
    notif_fd: RawFd,
    req: &SeccompNotif,
    wrapper_pid: Option<u32>,
    state: &Mutex<ObservationState>,
) {
    let nr = req.data.nr as libc::c_long;
    let pid = req.pid;
    let args = req.data.args;

    let paths: Vec<PathBuf> = if nr == libc::SYS_openat {
        resolve_at_path(pid, args[0] as i32, args[1])
            .into_iter()
            .collect()
    } else if nr == libc::SYS_openat2 {
        if read_open_how_is_write(pid, args[2]) {
            resolve_at_path(pid, args[0] as i32, args[1])
                .into_iter()
                .collect()
        } else {
            Vec::new()
        }
    } else if nr == libc::SYS_unlinkat {
        resolve_at_path(pid, args[0] as i32, args[1])
            .into_iter()
            .collect()
    } else if nr == libc::SYS_renameat || nr == libc::SYS_renameat2 {
        let mut out = Vec::with_capacity(2);
        if let Some(p) = resolve_at_path(pid, args[0] as i32, args[1]) {
            out.push(p);
        }
        if let Some(p) = resolve_at_path(pid, args[2] as i32, args[3]) {
            out.push(p);
        }
        out
    } else if nr == libc::SYS_truncate {
        resolve_at_path(pid, libc::AT_FDCWD, args[0])
            .into_iter()
            .collect()
    } else {
        #[cfg(target_arch = "x86_64")]
        {
            if nr == libc::SYS_open || nr == libc::SYS_creat || nr == libc::SYS_unlink {
                resolve_at_path(pid, libc::AT_FDCWD, args[0])
                    .into_iter()
                    .collect()
            } else if nr == libc::SYS_rename {
                let mut out = Vec::with_capacity(2);
                if let Some(p) = resolve_at_path(pid, libc::AT_FDCWD, args[0]) {
                    out.push(p);
                }
                if let Some(p) = resolve_at_path(pid, libc::AT_FDCWD, args[1]) {
                    out.push(p);
                }
                out
            } else {
                Vec::new()
            }
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            Vec::new()
        }
    };

    if paths.is_empty() {
        return;
    }
    // Verify notification ID is still valid before recording baselines (TOCTOU guard).
    let id = req.id;
    if unsafe { libc::ioctl(notif_fd, SECCOMP_IOCTL_NOTIF_ID_VALID, &id) } < 0 {
        return;
    }
    let is_vcs_diagnostic = is_vcs_diagnostic_writer(pid, wrapper_pid);
    let mut guard = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for path in paths {
        guard.record_pre_write_with_provenance(&path, is_vcs_diagnostic);
    }
}

/// Worktree setup may run checkout hooks with their own writers. Only inherit
/// provenance inside this observed process tree; sibling shell commands are
/// independent even if they write inside the new checkout.
fn is_vcs_diagnostic_writer(writer: u32, wrapper: Option<u32>) -> bool {
    let mut pid = writer;
    let mut inherited = false;
    loop {
        let Ok(stat) = fs::read(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // comm is parenthesized and may itself contain spaces or parentheses.
        let Some(end) = stat.iter().rposition(|&b| b == b')') else {
            return false;
        };
        if stat[..end].ends_with(b" (git") && is_vcs_diagnostic_git_process(pid, pid != writer) {
            if pid == writer {
                return true;
            }
            inherited = true;
        }
        if Some(pid) == wrapper {
            return inherited;
        }
        if wrapper.is_none() || pid == 1 {
            return false;
        }
        let Some(parent) = stat[end + 1..]
            .split(|b| b.is_ascii_whitespace())
            .filter(|word| !word.is_empty())
            .nth(1)
            .and_then(|word| std::str::from_utf8(word).ok()?.parse().ok())
        else {
            return false;
        };
        pid = parent;
    }
}

fn is_vcs_diagnostic_git_process(pid: u32, ancestor: bool) -> bool {
    let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let mut args = cmdline
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf8_lossy);
    let _ = args.next();
    let mut skip_next = false;
    while let Some(arg) = args.next() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(
            arg.as_ref(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        if arg == "worktree" {
            return args.next().is_some_and(|op| {
                matches!(
                    op.as_ref(),
                    "add" | "remove" | "move" | "repair" | "prune" | "lock" | "unlock"
                )
            });
        }
        return !ancestor
            && matches!(
                arg.as_ref(),
                "checkout"
                    | "switch"
                    | "restore"
                    | "reset"
                    | "merge"
                    | "rebase"
                    | "pull"
                    | "stash"
                    | "cherry-pick"
                    | "revert"
                    | "clean"
            );
    }
    false
}

fn read_open_how_is_write(pid: u32, how_ptr: u64) -> bool {
    if how_ptr == 0 {
        return false;
    }
    let mut buf = [0u8; 8];
    if !read_remote_bytes(pid, how_ptr, &mut buf) {
        return true;
    }
    let flags = u64::from_ne_bytes(buf);
    (flags & (WRITE_OPEN_FLAGS as u64)) != 0
}

fn resolve_at_path(pid: u32, dirfd: i32, path_ptr: u64) -> Option<PathBuf> {
    let raw = read_remote_cstring(pid, path_ptr)?;
    if raw.is_empty() {
        return None;
    }
    let p = Path::new(OsStr::from_bytes(&raw));
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    let base = if dirfd == libc::AT_FDCWD {
        fs::read_link(format!("/proc/{pid}/cwd")).ok()?
    } else {
        fs::read_link(format!("/proc/{pid}/fd/{dirfd}")).ok()?
    };
    Some(base.join(p))
}

fn read_remote_cstring(pid: u32, addr: u64) -> Option<Vec<u8>> {
    if addr == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    let mut offset = 0u64;
    while out.len() < 4096 {
        let n = read_remote_partial(pid, addr.wrapping_add(offset), &mut chunk);
        if n == 0 {
            break;
        }
        if let Some(nul) = chunk[..n].iter().position(|&b| b == 0) {
            out.extend_from_slice(&chunk[..nul]);
            return Some(out);
        }
        out.extend_from_slice(&chunk[..n]);
        offset += n as u64;
    }
    if out.is_empty() { None } else { Some(out) }
}

fn read_remote_bytes(pid: u32, addr: u64, dst: &mut [u8]) -> bool {
    read_remote_partial(pid, addr, dst) == dst.len()
}

fn read_remote_partial(pid: u32, addr: u64, dst: &mut [u8]) -> usize {
    let local = libc::iovec {
        iov_base: dst.as_mut_ptr().cast(),
        iov_len: dst.len(),
    };
    let remote = libc::iovec {
        iov_base: addr as *mut libc::c_void,
        iov_len: dst.len(),
    };
    let n = unsafe { libc::process_vm_readv(pid as libc::pid_t, &local, 1, &remote, 1, 0) };
    if n > 0 {
        return n as usize;
    }
    // Fallback to /proc/<pid>/mem when process_vm_readv crosses a page boundary or fails.
    use std::os::unix::fs::FileExt;
    let Ok(file) = fs::File::open(format!("/proc/{pid}/mem")) else {
        return 0;
    };
    file.read_at(dst, addr).unwrap_or(0)
}

fn send_fd(stream: &UnixStream, fd: RawFd) -> io::Result<()> {
    let mut data = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let cmsg_space = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast();
    msg.msg_controllen = cmsg_space;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as u32) as usize;
        std::ptr::copy_nonoverlapping(
            (&fd as *const RawFd).cast::<u8>(),
            libc::CMSG_DATA(cmsg),
            mem::size_of::<RawFd>(),
        );
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn recv_fd(stream: &UnixStream) -> io::Result<OwnedFd> {
    let mut data = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: data.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let cmsg_space = unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr().cast();
    msg.msg_controllen = cmsg_space;
    let n = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n <= 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if !cmsg.is_null()
            && (*cmsg).cmsg_level == libc::SOL_SOCKET
            && (*cmsg).cmsg_type == libc::SCM_RIGHTS
        {
            let mut fd: RawFd = -1;
            std::ptr::copy_nonoverlapping(
                libc::CMSG_DATA(cmsg),
                (&mut fd as *mut RawFd).cast::<u8>(),
                mem::size_of::<RawFd>(),
            );
            if fd >= 0 {
                return Ok(OwnedFd::from_raw_fd(fd));
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "missing SCM_RIGHTS fd",
    ))
}

/// Child wrapper entrypoint invoked via `<codex-exe> --codex-exec-observe <sock> -- <cmd> [args...]`.
/// Installs the `seccomp-unotify` write filter, hands off the listener fd to the host
/// supervisor, and `execvp`s the target command. Fails open if seccomp is unavailable.
pub fn run_observe_wrapper_main() -> ! {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // Expected: [argv0, "--codex-exec-observe", <sock_path>, "--", <cmd>, ...]
    let Some(sep) = args.iter().position(|a| a == "--") else {
        std::process::exit(126);
    };
    let sock_path = args.get(2).map(PathBuf::from);
    let cmd_args = &args[sep + 1..];
    if cmd_args.is_empty() {
        std::process::exit(126);
    }

    if let Some(sock_path) = sock_path {
        install_filter_and_handoff(&sock_path);
    }

    exec_target(cmd_args);
}

pub(crate) fn install_filter_and_handoff(sock_path: &Path) {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        let Ok(mut stream) = UnixStream::connect(sock_path) else {
            return;
        };
        let filter = build_write_filter();
        let prog = SockFprog {
            len: filter.len() as u16,
            filter: filter.as_ptr(),
        };
        let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if rc != 0 {
            return;
        }
        let fd = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                SECCOMP_SET_MODE_FILTER,
                SECCOMP_FILTER_FLAG_NEW_LISTENER,
                &prog as *const SockFprog,
            )
        };
        if fd < 0 {
            return;
        }
        let owned = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        if send_fd(&stream, owned.as_raw_fd()).is_ok() {
            let mut ack = [0u8; 1];
            let _ = stream.read_exact(&mut ack);
        }
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = sock_path;
    }
}

fn exec_target(cmd_args: &[std::ffi::OsString]) -> ! {
    let c_args: Vec<CString> = cmd_args
        .iter()
        .filter_map(|a| CString::new(a.as_bytes()).ok())
        .collect();
    if c_args.is_empty() {
        std::process::exit(126);
    }
    let mut ptrs: Vec<*const libc::c_char> = c_args.iter().map(|s| s.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe {
        libc::execvp(ptrs[0], ptrs.as_ptr());
    }
    std::process::exit(127);
}
