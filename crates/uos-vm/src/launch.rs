use std::fs::{self, DirBuilder, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{detect, AppKind, Guest, VmError, VmSpec, GUEST_APP_DIR};

const QEMU: &str = "qemu-system-x86_64";
const VIRTIOFSD: &str = "virtiofsd";
/// virtiofsd is not on PATH on most distributions.
const VIRTIOFSD_LOCATIONS: [&str; 3] = [
    "/usr/libexec/virtiofsd",
    "/usr/lib/qemu/virtiofsd",
    "/usr/lib/virtiofsd",
];
/// The unprivileged account the guest runs applications as. Files in the
/// share appear to belong to it.
const GUEST_APP_ID: u32 = 1000;
const SHARE_READY_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const PRIVATE_DIR_MODE: u32 = 0o700;
const CONSOLE_CHUNK: usize = 8192;
/// How much console output from before the application started is kept for
/// error messages.
const BOOT_LOG_LIMIT: usize = 16 * 1024;
/// Printed by the guest init on the console right before the application starts.
const START_MARKER: &[u8] = b"UOS_START";
/// Written by the guest init to its control port with the application's exit code.
const EXIT_MARKER: &str = "UOS_EXIT=";

/// Host locations the launcher depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub kernel: PathBuf,
    pub image_dir: PathBuf,
    pub kernel_release: String,
}

impl Paths {
    /// Uses the running host kernel and the per-user image directory.
    /// `UOS_KERNEL` and `UOS_IMAGE_DIR` override the defaults.
    pub fn from_env() -> Result<Self, VmError> {
        let kernel_release = fs::read_to_string("/proc/sys/kernel/osrelease")
            .map_err(VmError::io("could not read the kernel release"))?
            .trim()
            .to_string();
        let kernel = std::env::var_os("UOS_KERNEL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/lib/modules/{kernel_release}/vmlinuz")));
        let image_dir = std::env::var_os("UOS_IMAGE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(default_image_dir);
        Ok(Self {
            kernel,
            image_dir,
            kernel_release,
        })
    }

    pub fn image(&self, guest: Guest) -> PathBuf {
        self.image_dir.join(format!("{}.ext4", guest.name()))
    }
}

fn default_image_dir() -> PathBuf {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("/var/lib"));
    data_home.join("uos/images")
}

#[derive(Debug, Clone, Copy)]
pub struct LaunchRequest<'a> {
    pub app: &'a Path,
    pub args: &'a [String],
    pub memory_mib: u32,
    pub cpus: u32,
    /// Connects the caller's keyboard to the application.
    pub interactive: bool,
    /// Stops the VM if the application runs longer than this.
    pub timeout: Option<Duration>,
    /// Lets the application change files in its own directory. Without it
    /// the directory is shared read-only.
    pub writable_share: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaunchOutcome {
    pub kind: AppKind,
    pub exit_code: i32,
}

/// Boots a fresh VM, runs the application in it and writes what the
/// application prints to `output`.
pub fn launch(
    request: &LaunchRequest<'_>,
    paths: &Paths,
    output: &mut dyn Write,
) -> Result<LaunchOutcome, VmError> {
    let app = request
        .app
        .canonicalize()
        .map_err(VmError::io(format!("could not find {}", request.app.display())))?;
    let kind = detect(&app)?;
    let guest = kind.guest();
    let image = paths.image(guest);
    check_image(&image, &paths.kernel_release, guest)?;
    let (share_dir, file_name) = match (app.parent(), app.file_name()) {
        (Some(dir), Some(name)) => (dir, name.to_string_lossy()),
        _ => return Err(VmError::Unsupported(app.clone())),
    };
    let home = std::env::var_os("HOME").and_then(|home| PathBuf::from(home).canonicalize().ok());
    check_share_dir(&app, share_dir, home.as_deref())?;

    let run_dir = RunDir::create()?;
    let share_socket = run_dir.path.join("fs.sock");
    let _share = ShareHelper::start(share_dir, &share_socket, &run_dir.path, request.writable_share)?;

    let spec = VmSpec {
        kernel: paths.kernel.clone(),
        image,
        share_socket,
        control_log: run_dir.path.join("control"),
        memory_mib: request.memory_mib,
        cpus: request.cpus,
        guest_argv: guest_argv(kind, &file_name, request.args),
    };
    let exit_code = run_qemu(&spec, request, &run_dir.path, output)?;
    Ok(LaunchOutcome { kind, exit_code })
}

/// The command line the guest runs for an application shared at `/app`.
fn guest_argv(kind: AppKind, file_name: &str, args: &[String]) -> Vec<String> {
    let program = format!("{GUEST_APP_DIR}/{file_name}");
    let prefix = match kind {
        AppKind::LinuxElf | AppKind::Script => None,
        AppKind::WindowsPe => Some("wine".to_string()),
    };
    prefix
        .into_iter()
        .chain([program])
        .chain(args.iter().cloned())
        .collect()
}

/// The application's directory is shared with the guest, so it must not be
/// the home directory or something that contains it.
fn check_share_dir(app: &Path, share_dir: &Path, home: Option<&Path>) -> Result<(), VmError> {
    let holds_home = home.is_some_and(|home| home.starts_with(share_dir));
    if holds_home || share_dir.parent().is_none() {
        return Err(VmError::ShareTooBroad {
            app: app.to_path_buf(),
            dir: share_dir.to_path_buf(),
        });
    }
    Ok(())
}

/// The image carries kernel modules, so it must match the kernel being booted.
fn check_image(image: &Path, running: &str, guest: Guest) -> Result<(), VmError> {
    let missing = || VmError::ImageMissing {
        image: image.to_path_buf(),
        guest: guest.name(),
    };
    if !image.is_file() {
        return Err(missing());
    }
    let stamp = image.with_extension("kernel");
    let built_for = fs::read_to_string(stamp).map_err(|_| missing())?;
    if built_for.trim() != running {
        return Err(VmError::KernelMismatch {
            image: image.to_path_buf(),
            built_for: built_for.trim().to_string(),
            running: running.to_string(),
            guest: guest.name(),
        });
    }
    Ok(())
}

fn run_qemu(
    spec: &VmSpec,
    request: &LaunchRequest<'_>,
    run_dir: &Path,
    output: &mut dyn Write,
) -> Result<i32, VmError> {
    let log_path = run_dir.join("qemu.log");
    let log = File::create(&log_path).map_err(VmError::io("could not create the VM log"))?;
    let stdin = if request.interactive {
        Stdio::inherit()
    } else {
        Stdio::null()
    };
    let mut child = Command::new(QEMU)
        .args(spec.qemu_args()?)
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()
        .map_err(|source| match source.kind() {
            io::ErrorKind::NotFound => VmError::MissingHostTool(QEMU),
            _ => VmError::Io {
                context: "could not start the virtual machine".into(),
                source,
            },
        })?;
    let console = child.stdout.take();
    let vm = RunningVm::watch(child, request.timeout);

    let mut filter = ConsoleFilter::default();
    if let Some(console) = console {
        // An error here drops `vm`, which stops the machine.
        forward_console(console, &mut filter, output)?;
    }
    let status = vm.wait()?;
    if let Some(limit) = request.timeout.filter(|_| vm.timed_out()) {
        return Err(VmError::TimedOut(limit.as_secs()));
    }
    // Read as bytes: the kernel console shares this file and need not be UTF-8.
    let report = fs::read(&spec.control_log).unwrap_or_default();
    let report = String::from_utf8_lossy(&report);
    parse_exit_report(&report).ok_or_else(|| {
        let log = fs::read_to_string(&log_path).unwrap_or_default();
        let details = [log.trim(), report.trim(), filter.boot_log().trim()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        VmError::NoExitCode(format!("{status}; {details}"))
    })
}

fn forward_console(
    mut console: impl Read,
    filter: &mut ConsoleFilter,
    output: &mut dyn Write,
) -> Result<(), VmError> {
    let mut chunk = [0u8; CONSOLE_CHUNK];
    loop {
        let read = match console.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(VmError::io("could not read the VM console")(error)),
        };
        let shown = filter.feed(&chunk[..read]);
        output
            .write_all(&shown)
            .and_then(|()| output.flush())
            .map_err(VmError::io("could not write application output"))?;
    }
}

/// The last `UOS_EXIT=<code>` line the guest init wrote to its control port,
/// which it shares with kernel messages.
fn parse_exit_report(report: &str) -> Option<i32> {
    report
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix(EXIT_MARKER)?.parse().ok())
}

/// Hides what the guest prints while booting; everything after the start
/// marker is the application's own output and passes through untouched.
#[derive(Debug, Default)]
struct ConsoleFilter {
    started: bool,
    boot: Vec<u8>,
}

impl ConsoleFilter {
    /// Returns the bytes of this chunk to show the user.
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.started {
            return chunk.to_vec();
        }
        self.boot.extend_from_slice(chunk);
        let Some(after_marker) = end_of_start_line(&self.boot) else {
            let excess = self.boot.len().saturating_sub(BOOT_LOG_LIMIT);
            self.boot.drain(..excess);
            return Vec::new();
        };
        self.started = true;
        self.boot.split_off(after_marker)
    }

    fn boot_log(&self) -> String {
        String::from_utf8_lossy(&self.boot).into_owned()
    }
}

/// Index just past the start marker's line, once the whole line has arrived.
fn end_of_start_line(console: &[u8]) -> Option<usize> {
    let marker = console
        .windows(START_MARKER.len())
        .position(|window| window == START_MARKER)?;
    let line_break = console[marker..].iter().position(|byte| *byte == b'\n')?;
    Some(marker + line_break + 1)
}

/// A started QEMU process. It is killed when this is dropped, and by a
/// watchdog thread if it outlives its time limit.
struct RunningVm {
    child: Arc<Mutex<Child>>,
    finished: Arc<AtomicBool>,
    timed_out: Arc<AtomicBool>,
}

impl RunningVm {
    fn watch(child: Child, timeout: Option<Duration>) -> Self {
        let vm = Self {
            child: Arc::new(Mutex::new(child)),
            finished: Arc::new(AtomicBool::new(false)),
            timed_out: Arc::new(AtomicBool::new(false)),
        };
        if let Some(limit) = timeout {
            let (child, finished, timed_out) = (
                Arc::clone(&vm.child),
                Arc::clone(&vm.finished),
                Arc::clone(&vm.timed_out),
            );
            thread::spawn(move || {
                let deadline = Instant::now() + limit;
                while Instant::now() < deadline {
                    if finished.load(Ordering::SeqCst) {
                        return;
                    }
                    thread::sleep(POLL_INTERVAL);
                }
                timed_out.store(true, Ordering::SeqCst);
                // Killing an already-exited process fails harmlessly.
                let _ = lock(&child).kill();
            });
        }
        vm
    }

    /// Waits for the process to exit. Call once the console has closed, so
    /// the watchdog is never kept waiting for the lock.
    fn wait(&self) -> Result<ExitStatus, VmError> {
        let status = lock(&self.child)
            .wait()
            .map_err(VmError::io("could not wait for the virtual machine"));
        self.finished.store(true, Ordering::SeqCst);
        status
    }

    fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }
}

impl Drop for RunningVm {
    fn drop(&mut self) {
        self.finished.store(true, Ordering::SeqCst);
        let mut child = lock(&self.child);
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn lock(child: &Mutex<Child>) -> MutexGuard<'_, Child> {
    // A panic elsewhere must not stop us from reaping the process.
    child.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Per-launch scratch directory only this user can enter, removed when the
/// launch ends.
struct RunDir {
    path: PathBuf,
}

impl RunDir {
    fn create() -> Result<Self, VmError> {
        let uid = fs::metadata("/proc/self")
            .map_err(VmError::io("could not read the current user"))?
            .uid();
        let base = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(runtime) => PathBuf::from(runtime).join("uos"),
            None => std::env::temp_dir().join(format!("uos-{uid}")),
        };
        ensure_private_dir(&base, uid)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        let path = base.join(format!("vm-{}-{nanos}", std::process::id()));
        DirBuilder::new()
            .mode(PRIVATE_DIR_MODE)
            .create(&path)
            .map_err(VmError::io("could not create the VM run directory"))?;
        Ok(Self { path })
    }
}

/// Creates `dir` for this user alone. An existing one must be a real
/// directory owned by `uid`; it is closed to everyone else if it was not.
fn ensure_private_dir(dir: &Path, uid: u32) -> Result<(), VmError> {
    match DirBuilder::new().mode(PRIVATE_DIR_MODE).create(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(dir)
                .map_err(VmError::io(format!("could not inspect {}", dir.display())))?;
            if !meta.is_dir() || meta.uid() != uid {
                return Err(VmError::UnsafeRunDir(dir.to_path_buf()));
            }
            fs::set_permissions(dir, fs::Permissions::from_mode(PRIVATE_DIR_MODE))
                .map_err(VmError::io(format!("could not restrict {}", dir.display())))
        }
        Err(error) => Err(VmError::io(format!("could not create {}", dir.display()))(error)),
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        // Best effort: a leftover run directory is harmless.
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// The virtiofsd process that shares the application's directory.
struct ShareHelper {
    child: Child,
}

impl ShareHelper {
    fn start(share_dir: &Path, socket: &Path, run_dir: &Path, writable: bool) -> Result<Self, VmError> {
        let log_path = run_dir.join("virtiofsd.log");
        let log = File::create(&log_path).map_err(VmError::io("could not create the share log"))?;
        let program = VIRTIOFSD_LOCATIONS
            .iter()
            .map(Path::new)
            .find(|candidate| candidate.is_file())
            .unwrap_or(Path::new(VIRTIOFSD));
        // virtiofsd runs in a user namespace where this user is id 0; present
        // those files to the guest as its application account's.
        let id_map = format!("map:{GUEST_APP_ID}:0:1");
        let mut command = Command::new(program);
        command
            .arg("--socket-path")
            .arg(socket)
            .arg("--shared-dir")
            .arg(share_dir)
            .args(["--translate-uid", &id_map, "--translate-gid", &id_map]);
        if !writable {
            command.arg("--readonly");
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|source| match source.kind() {
                io::ErrorKind::NotFound => VmError::MissingHostTool(VIRTIOFSD),
                _ => VmError::Io {
                    context: "could not start the file-sharing helper".into(),
                    source,
                },
            })?;
        let mut helper = Self { child };
        helper.wait_until_ready(socket, &log_path)?;
        Ok(helper)
    }

    fn wait_until_ready(&mut self, socket: &Path, log_path: &Path) -> Result<(), VmError> {
        let deadline = Instant::now() + SHARE_READY_TIMEOUT;
        while !socket.exists() {
            let exited = matches!(self.child.try_wait(), Ok(Some(_)) | Err(_));
            if exited || Instant::now() >= deadline {
                let log = fs::read_to_string(log_path).unwrap_or_default();
                return Err(VmError::ShareNotReady(log.trim().to_string()));
            }
            thread::sleep(POLL_INTERVAL);
        }
        Ok(())
    }
}

impl Drop for ShareHelper {
    fn drop(&mut self) {
        // virtiofsd normally exits when the VM disconnects; make sure of it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(chunks: &[&[u8]]) -> (Vec<u8>, ConsoleFilter) {
        let mut filter = ConsoleFilter::default();
        let shown = chunks.iter().flat_map(|chunk| filter.feed(chunk)).collect();
        (shown, filter)
    }

    #[test]
    fn forwards_only_what_follows_the_start_marker() {
        let (shown, filter) = feed_all(&[b"boot noise\nUOS_START\nhello\nwor", b"ld"]);

        assert_eq!(shown, b"hello\nworld");
        assert_eq!(filter.boot_log(), "boot noise\nUOS_START\n");
    }

    #[test]
    fn finds_a_start_marker_split_across_chunks() {
        let (shown, _) = feed_all(&[b"noise UOS_ST", b"ART\r", b"\nout"]);

        assert_eq!(shown, b"out");
    }

    #[test]
    fn passes_application_output_through_unchanged() {
        let (shown, _) = feed_all(&[b"UOS_START\n", b"UOS_EXIT=0\n\xff\r\nUOS_START\n"]);

        assert_eq!(shown, b"UOS_EXIT=0\n\xff\r\nUOS_START\n");
    }

    #[test]
    fn keeps_only_the_end_of_a_long_boot_log() {
        let noise = vec![b'x'; BOOT_LOG_LIMIT];

        let (shown, filter) = feed_all(&[&noise, b"panic: no init"]);

        assert!(shown.is_empty());
        assert_eq!(filter.boot.len(), BOOT_LOG_LIMIT);
        assert!(filter.boot_log().ends_with("panic: no init"));
    }

    #[test]
    fn forwards_a_console_stream_to_the_output() {
        let console: &[u8] = b"UOS_START\nout\n";
        let mut filter = ConsoleFilter::default();
        let mut shown = Vec::new();

        forward_console(console, &mut filter, &mut shown).unwrap();

        assert_eq!(shown, b"out\n");
    }

    #[test]
    fn reads_the_exit_code_from_the_control_report() {
        assert_eq!(parse_exit_report("UOS_EXIT=42\r\n"), Some(42));
        assert_eq!(parse_exit_report("UOS_EXIT=1\nUOS_EXIT=7\n"), Some(7));
        assert_eq!(
            parse_exit_report("UOS_EXIT=3\r\n[  0.5] sysrq: Power Off\r\n"),
            Some(3)
        );
    }

    #[test]
    fn reports_no_exit_code_for_an_empty_or_malformed_report() {
        assert_eq!(parse_exit_report(""), None);
        assert_eq!(parse_exit_report("UOS_EXIT=abc\n"), None);
    }

    #[test]
    fn refuses_to_share_the_home_directory_or_its_ancestors() {
        let home = Path::new("/home/u");
        let too_broad = |dir: &str| {
            check_share_dir(Path::new("/x/app"), Path::new(dir), Some(home)).is_err()
        };

        assert!(too_broad("/home/u"));
        assert!(too_broad("/home"));
        assert!(too_broad("/"));
        assert!(!too_broad("/home/u/Downloads"));
        assert!(!too_broad("/usr/bin"));
    }

    #[test]
    fn refuses_to_share_the_root_directory_when_home_is_unknown() {
        assert!(check_share_dir(Path::new("/app"), Path::new("/"), None).is_err());
        assert!(check_share_dir(Path::new("/opt/x/app"), Path::new("/opt/x"), None).is_ok());
    }

    #[test]
    fn closes_an_existing_directory_to_other_users() {
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let dir = std::env::temp_dir().join(format!("uos-vm-test-{}-private", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_dir(&dir, uid).unwrap();

        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn rejects_a_run_directory_that_is_not_ours_or_not_a_directory() {
        let uid = fs::metadata("/proc/self").unwrap().uid();
        let dir = std::env::temp_dir().join(format!("uos-vm-test-{}-foreign", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let link = dir.join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();

        let wrong_owner = ensure_private_dir(&dir, uid + 1);
        let symlink = ensure_private_dir(&link, uid);

        assert!(matches!(wrong_owner, Err(VmError::UnsafeRunDir(_))));
        assert!(matches!(symlink, Err(VmError::UnsafeRunDir(_))));
    }

    #[test]
    fn kills_a_process_that_outlives_its_time_limit() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let started = Instant::now();

        let vm = RunningVm::watch(child, Some(Duration::from_millis(100)));
        // Stands in for reading the console until the process closes it.
        while !vm.timed_out() {
            thread::sleep(POLL_INTERVAL);
        }
        let status = vm.wait().unwrap();

        assert!(!status.success());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn kills_the_process_when_dropped() {
        let child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();

        drop(RunningVm::watch(child, None));

        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn runs_linux_programs_directly() {
        let argv = guest_argv(AppKind::LinuxElf, "tool", &["-v".to_string()]);

        assert_eq!(argv, ["/app/tool", "-v"]);
    }

    #[test]
    fn runs_windows_programs_through_wine() {
        let argv = guest_argv(AppKind::WindowsPe, "setup.exe", &[]);

        assert_eq!(argv, ["wine", "/app/setup.exe"]);
    }

    fn temp_image(name: &str, kernel_stamp: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("uos-vm-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let image = dir.join("linux.ext4");
        fs::write(&image, b"").unwrap();
        if let Some(stamp) = kernel_stamp {
            fs::write(dir.join("linux.kernel"), stamp).unwrap();
        }
        image
    }

    #[test]
    fn accepts_an_image_built_for_the_running_kernel() {
        let image = temp_image("match", Some("7.2.13\n"));

        assert!(check_image(&image, "7.2.13", Guest::Linux).is_ok());
    }

    #[test]
    fn rejects_an_image_built_for_another_kernel() {
        let image = temp_image("mismatch", Some("7.1.0\n"));

        let result = check_image(&image, "7.2.13", Guest::Linux);

        assert!(matches!(result, Err(VmError::KernelMismatch { .. })));
    }

    #[test]
    fn reports_a_missing_image_with_the_build_command() {
        let result = check_image(Path::new("/nonexistent/linux.ext4"), "7.2.13", Guest::Linux);

        let message = result.unwrap_err().to_string();
        assert!(message.contains("guest/build-image.sh linux"), "{message}");
    }

    #[test]
    fn treats_an_image_without_a_kernel_stamp_as_missing() {
        let image = temp_image("nostamp", None);

        let result = check_image(&image, "7.2.13", Guest::Linux);

        assert!(matches!(result, Err(VmError::ImageMissing { .. })));
    }

    #[test]
    fn names_images_after_their_guest() {
        let paths = Paths {
            kernel: "/k".into(),
            image_dir: "/images".into(),
            kernel_release: "r".into(),
        };

        assert_eq!(paths.image(Guest::Windows), Path::new("/images/windows.ext4"));
    }
}
