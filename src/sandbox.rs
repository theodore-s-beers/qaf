use anyhow::{Context, Result, ensure};
use std::{
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;

/// Supervisor-owned files are outside the worker's writable scratch directory.
pub struct Sandbox {
    project: PathBuf,
    credentials: PathBuf,
    runtime: tempfile::TempDir,
    scratch: PathBuf,
    worker: PathBuf,
    protected: Vec<PathBuf>,
}

impl Sandbox {
    pub fn new(project: &Path, credentials: &Path, executable: &Path) -> Result<Self> {
        let project = project
            .canonicalize()
            .context("resolve project directory")?;
        ensure!(
            project.is_dir() && project.parent().is_some(),
            "select a project directory, not the filesystem root"
        );
        let credentials = resolve(credentials)?;
        let runtime = tempfile::Builder::new().prefix("qaf-run-").tempdir()?;
        let root = runtime.path().canonicalize()?;
        let scratch = root.join("scratch");
        std::fs::create_dir(&scratch)?;
        let worker = root.join("worker");
        std::fs::copy(executable, &worker).context("prepare isolated file-tool worker")?;
        let mut protected = Vec::new();
        for name in [".git", ".jj"] {
            let path = project.join(name);
            if path.exists() {
                protected.push(path.canonicalize()?);
            }
        }
        Ok(Self {
            project,
            credentials,
            runtime,
            scratch,
            worker,
            protected,
        })
    }

    pub fn project(&self) -> &Path {
        &self.project
    }
    pub fn file_command(
        &self,
        payload: &[u8],
        unrestricted: bool,
    ) -> Result<(Command, tempfile::NamedTempFile)> {
        let mut request = tempfile::NamedTempFile::new_in(self.runtime.path())?;
        request.write_all(payload)?;
        request.flush()?;
        let mut command = self.command(&self.worker, unrestricted)?;
        command.arg("__tool-worker").arg(request.path());
        Ok((command, request))
    }

    pub fn shell_command(&self, script: &str, unrestricted: bool) -> Result<Command> {
        let mut command = self.command(Path::new("/bin/sh"), unrestricted)?;
        command.arg("-c").arg(script);
        Ok(command)
    }

    pub async fn check(&self) -> Result<()> {
        let mut command = self.shell_command("true", false)?;
        let output = tokio::time::timeout(std::time::Duration::from_secs(10), command.output())
            .await.context("sandbox startup timed out")?
            .context("sandbox unavailable: macOS requires /usr/bin/sandbox-exec; Linux requires bubblewrap and enabled user namespaces")?;
        ensure!(
            output.status.success(),
            "sandbox startup failed; refusing unrestricted execution: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn command(&self, program: &Path, unrestricted: bool) -> Result<Command> {
        let mut command = if unrestricted {
            Command::new(program)
        } else {
            self.restricted_command(program)?
        };
        command
            .current_dir(&self.project)
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Do not inherit authentication tokens, agent sockets, or loader injection.
        for name in [
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "LANG",
            "LC_ALL",
            "TZ",
            "RUSTUP_HOME",
            "CARGO_HOME",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
            .env("TMPDIR", &self.scratch)
            .env("TMP", &self.scratch)
            .env("TEMP", &self.scratch);
        if unrestricted || cfg!(target_os = "macos") {
            command.stdin(Stdio::null());
        }
        #[cfg(unix)]
        command.process_group(0);
        Ok(command)
    }

    #[cfg(target_os = "macos")]
    fn restricted_command(&self, program: &Path) -> Result<Command> {
        use std::fmt::Write as _;
        let mut policy = String::from(
            r#"(version 1)
(deny default)
(allow process-exec process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow file-read*)
(deny file-read* (regex #"^/dev/(tty|pts)"))
(allow file-write* (subpath (param "PROJECT")) (subpath (param "SCRATCH")))
(allow file-write-data (literal "/dev/null"))
(allow sysctl-read (sysctl-name-prefix "hw.") (sysctl-name-prefix "kern.os")
    (sysctl-name "kern.argmax") (sysctl-name "kern.hostname")
    (sysctl-name "kern.maxfilesperproc") (sysctl-name "kern.maxproc")
    (sysctl-name "kern.version") (sysctl-name "machdep.cpu.brand_string"))
(allow mach-host*)
(allow mach-lookup (global-name "com.apple.system.opendirectoryd.libinfo"))
(deny file-read* file-write* (subpath (param "CREDENTIALS")))
"#,
        );
        let mut command = Command::new("/usr/bin/sandbox-exec");
        for (name, path) in [
            ("PROJECT", &self.project),
            ("SCRATCH", &self.scratch),
            ("CREDENTIALS", &self.credentials),
        ] {
            command.arg("-D").arg(parameter(name, path));
        }
        policy.push_str("(deny file-write* (literal (param \"WORKER\")))\n");
        command.arg("-D").arg(parameter("WORKER", &self.worker));
        for (index, path) in self.protected.iter().enumerate() {
            let name = format!("PROTECTED_{index}");
            writeln!(policy, "(deny file-write* (subpath (param \"{name}\")))")?;
            command.arg("-D").arg(parameter(&name, path));
        }
        // Runtime is outside the project and only scratch is writable by default.
        ensure!(
            !self
                .runtime
                .path()
                .canonicalize()?
                .starts_with(&self.project),
            "project contains qaf runtime; select a narrower project directory"
        );
        command.arg("-p").arg(policy).arg(program);
        Ok(command)
    }

    #[cfg(target_os = "linux")]
    fn restricted_command(&self, program: &Path) -> Result<Command> {
        use std::io::{Seek, SeekFrom};

        let launcher = ["/usr/bin/bwrap", "/bin/bwrap"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .context(
                "bubblewrap is not installed; install your distribution's bubblewrap package",
            )?;
        let mut command = Command::new(launcher);
        command.args([
            "--unshare-user",
            "--unshare-ipc",
            "--unshare-pid",
            "--unshare-net",
            "--unshare-uts",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/run",
            "--tmpfs",
            "/var/tmp",
        ]);
        command.arg("--bind").arg(&self.project).arg(&self.project);
        let runtime = self.runtime.path().canonicalize()?;
        command.arg("--ro-bind").arg(&runtime).arg(&runtime);
        command.arg("--bind").arg(&self.scratch).arg(&self.scratch);
        for path in &self.protected {
            command.arg("--ro-bind").arg(path).arg(path);
        }
        if self.credentials.exists() {
            ensure!(
                self.credentials.is_dir(),
                "credential path must be a directory"
            );
            command.arg("--tmpfs").arg(&self.credentials);
            command.arg("--remount-ro").arg(&self.credentials);
        }
        // Hide host temporary files without granting additional writable roots.
        for path in ["/tmp", "/run", "/var/tmp"] {
            command.arg("--remount-ro").arg(path);
        }
        // A network namespace does not block host filesystem Unix sockets.
        // Block socket creation too; the worker receives no inherited sockets.
        let mut filter = tempfile::tempfile()?;
        filter.write_all(&socket_filter()?)?;
        filter.seek(SeekFrom::Start(0))?;
        command.args(["--seccomp", "0"]).stdin(Stdio::from(filter));
        command
            .arg("--chdir")
            .arg(&self.project)
            .arg("--")
            .arg(program);
        Ok(command)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn restricted_command(&self, _: &Path) -> Result<Command> {
        anyhow::bail!("qaf sandboxing supports only macOS and Linux")
    }
}

#[cfg(target_os = "macos")]
fn parameter(name: &str, path: &Path) -> std::ffi::OsString {
    let mut value = std::ffi::OsString::from(format!("{name}="));
    value.push(path);
    value
}

/// Resolve existing ancestors so paths under /tmp aliases and symlinks agree.
fn resolve(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path.canonicalize().context("resolve protected path");
    }
    let absolute = std::path::absolute(path)?;
    let parent = absolute.parent().context("protected path has no parent")?;
    Ok(resolve(parent)?.join(absolute.file_name().context("protected path has no name")?))
}

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
fn socket_filter() -> Result<Vec<u8>> {
    #[cfg(target_arch = "x86_64")]
    let arch = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    let arch = 0xc000_00b7;

    // Classic BPF over seccomp_data: validate architecture, reject x32 ABI,
    // Deny socket creation, including io_uring's alternative socket operations.
    let instructions: [(u16, u8, u8, u32); 11] = [
        (0x20, 0, 0, 4),
        (0x15, 1, 0, arch),
        (0x06, 0, 0, 0x8000_0000),
        (0x20, 0, 0, 0),
        (0x35, 0, 1, 0x4000_0000),
        (0x06, 0, 0, 0x8000_0000),
        (0x15, 0, 1, u32::try_from(libc::SYS_socket)?),
        (0x06, 0, 0, 0x0005_0001),
        (0x15, 0, 1, u32::try_from(libc::SYS_io_uring_setup)?),
        (0x06, 0, 0, 0x0005_0001),
        (0x06, 0, 0, 0x7fff_0000),
    ];
    Ok(instructions
        .into_iter()
        .flat_map(|(code, jt, jf, k)| {
            code.to_ne_bytes()
                .into_iter()
                .chain([jt, jf])
                .chain(k.to_ne_bytes())
        })
        .collect())
}

#[cfg(all(
    target_os = "linux",
    not(any(target_arch = "x86_64", target_arch = "aarch64"))
))]
fn socket_filter() -> Result<Vec<u8>> {
    anyhow::bail!("Linux sandbox supports x86_64 and aarch64")
}
