use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};
use uos_llm::ToolSpec;
use uos_vm::{LaunchRequest, Paths};

use crate::risk::is_sensitive_path;
use crate::{classify_command, Risk};

/// Most bytes of a file, a command's output or an application's output that
/// are kept and handed back to the model.
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const COMMAND_TIMEOUT_SECS: &str = "60";
const KILL_AFTER_SECS: &str = "5";
/// Read-only commands run without asking, so they resolve programs here and
/// not in directories the user can write to.
const TRUSTED_PATH: &str = "/usr/bin:/bin";
const APP_TIMEOUT: Duration = Duration::from_secs(120);
const APP_MEMORY_MIB: u32 = 1024;
const APP_CPUS: u32 = 2;
/// Top-level home entry that is on the user's PATH.
const HOME_BIN: &str = "bin";

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("missing or invalid argument '{0}'")]
    BadArgument(&'static str),
    #[error("{0}")]
    Failed(String),
}

/// One capability the model can call.
pub trait Tool {
    fn spec(&self) -> ToolSpec;
    /// Rated from the actual arguments, before anything runs.
    fn risk(&self, args: &Value) -> Risk;
    fn run(&self, args: &Value) -> Result<String, ToolError>;
}

pub fn default_tools(vm_paths: Paths) -> Vec<Box<dyn Tool>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    vec![
        Box::new(RunCommand),
        Box::new(ReadFile),
        Box::new(WriteFile { home }),
        Box::new(HardwareStatus),
        Box::new(LaunchApp { paths: vm_paths }),
    ]
}

fn text_arg<'a>(args: &'a Value, key: &'static str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or(ToolError::BadArgument(key))
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required })
}

/// Reads at most [`MAX_OUTPUT_BYTES`] and says so if there was more.
fn read_capped(mut source: impl Read) -> io::Result<String> {
    let mut kept = Vec::new();
    source
        .by_ref()
        .take(MAX_OUTPUT_BYTES as u64)
        .read_to_end(&mut kept)?;
    // Keep draining so a writer on the other end is not left blocked.
    let dropped = io::copy(&mut source, &mut io::sink())?;
    let text = String::from_utf8_lossy(&kept);
    Ok(if dropped > 0 {
        format!("{text}\n[truncated: {dropped} more bytes]")
    } else {
        text.into_owned()
    })
}

fn read_capped_in_background(source: impl Read + Send + 'static) -> JoinHandle<io::Result<String>> {
    thread::spawn(move || read_capped(source))
}

/// A sink that keeps the first [`MAX_OUTPUT_BYTES`] and counts the rest.
#[derive(Default)]
struct CappedBuffer {
    kept: Vec<u8>,
    dropped: usize,
}

impl Write for CappedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = MAX_OUTPUT_BYTES.saturating_sub(self.kept.len()).min(bytes.len());
        self.kept.extend_from_slice(&bytes[..room]);
        self.dropped += bytes.len() - room;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl CappedBuffer {
    fn text(&self) -> String {
        let text = String::from_utf8_lossy(&self.kept);
        if self.dropped > 0 {
            format!("{text}\n[truncated: {} more bytes]", self.dropped)
        } else {
            text.into_owned()
        }
    }
}

struct RunCommand;

impl Tool for RunCommand {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command",
            description: "Run a shell command on the host and return its output and exit code.",
            parameters: object_schema(
                json!({ "command": { "type": "string", "description": "Shell command line" } }),
                &["command"],
            ),
        }
    }

    fn risk(&self, args: &Value) -> Risk {
        text_arg(args, "command").map_or(Risk::Privileged, classify_command)
    }

    fn run(&self, args: &Value) -> Result<String, ToolError> {
        let command = text_arg(args, "command")?;
        let failed = |e: io::Error| ToolError::Failed(format!("could not run the command: {e}"));
        let mut shell = Command::new("timeout");
        shell
            .args(["--kill-after", KILL_AFTER_SECS, COMMAND_TIMEOUT_SECS, "sh", "-c", command])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if classify_command(command) == Risk::Observe {
            shell.env("PATH", TRUSTED_PATH);
        }
        let mut child = shell.spawn().map_err(failed)?;
        let readers = (child.stdout.take(), child.stderr.take());
        let (Some(stdout), Some(stderr)) = readers else {
            return Err(ToolError::Failed("could not capture the command's output".into()));
        };
        let errors = read_capped_in_background(stderr);
        let output = read_capped(stdout).map_err(failed)?;
        let errors = errors
            .join()
            .map_err(|_| ToolError::Failed("could not capture the command's errors".into()))?
            .map_err(failed)?;
        let status = child.wait().map_err(failed)?;
        Ok(format!("{status}\n{output}{errors}"))
    }
}

struct ReadFile;

impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file",
            description: "Read a text file on the host.",
            parameters: object_schema(json!({ "path": { "type": "string" } }), &["path"]),
        }
    }

    /// Hidden files and credential stores ask first, wherever a symlink or
    /// `..` in the given path actually leads.
    fn risk(&self, args: &Value) -> Risk {
        let resolved = text_arg(args, "path")
            .ok()
            .and_then(|path| Path::new(path).canonicalize().ok());
        match resolved {
            Some(path) if !is_sensitive_path(&path.to_string_lossy()) => Risk::Observe,
            Some(_) | None => Risk::Privileged,
        }
    }

    fn run(&self, args: &Value) -> Result<String, ToolError> {
        let path = text_arg(args, "path")?;
        let failed = |e: io::Error| ToolError::Failed(format!("{path}: {e}"));
        let file = File::open(path).map_err(failed)?;
        // Devices and pipes can be endless or block forever.
        if !file.metadata().map_err(failed)?.is_file() {
            return Err(ToolError::Failed(format!("{path}: not a regular file")));
        }
        read_capped(file).map_err(failed)
    }
}

struct WriteFile {
    home: Option<PathBuf>,
}

impl WriteFile {
    /// `Modify` only for an ordinary, visible file under the home directory
    /// that nothing else links to. Hidden entries hold settings and startup
    /// scripts, so they and everything outside home are `Privileged`, as is
    /// any path the location of which cannot be confirmed.
    fn risk_of(&self, path: &Path) -> Risk {
        let Some(home) = self.home.as_deref().and_then(|h| h.canonicalize().ok()) else {
            return Risk::Privileged;
        };
        // Resolve the directory so a symlink cannot point the write elsewhere.
        let resolved_dir = path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .canonicalize();
        let (Ok(dir), Some(name)) = (resolved_dir, path.file_name()) else {
            return Risk::Privileged;
        };
        let target = dir.join(name);
        let Ok(inside_home) = target.strip_prefix(&home) else {
            return Risk::Privileged;
        };
        let on_path = inside_home.components().next() == Some(Component::Normal(HOME_BIN.as_ref()));
        // A symlink or a second hard link means the bytes land in a file
        // known by another name.
        let aliased = target
            .symlink_metadata()
            .is_ok_and(|meta| !meta.is_file() || meta.nlink() > 1);
        if on_path || aliased || is_sensitive_path(&inside_home.to_string_lossy()) {
            Risk::Privileged
        } else {
            Risk::Modify
        }
    }
}

impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file",
            description: "Create or replace a text file on the host with the given content.",
            parameters: object_schema(
                json!({ "path": { "type": "string" }, "content": { "type": "string" } }),
                &["path", "content"],
            ),
        }
    }

    fn risk(&self, args: &Value) -> Risk {
        text_arg(args, "path").map_or(Risk::Privileged, |path| self.risk_of(Path::new(path)))
    }

    fn run(&self, args: &Value) -> Result<String, ToolError> {
        let path = text_arg(args, "path")?;
        let content = text_arg(args, "content")?;
        fs::write(path, content).map_err(|e| ToolError::Failed(format!("{path}: {e}")))?;
        Ok(format!("wrote {} bytes to {path}", content.len()))
    }
}

struct HardwareStatus;

impl Tool for HardwareStatus {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "hardware_status",
            description: "Report GPU memory, RAM and CPU core count of this machine.",
            parameters: object_schema(json!({}), &[]),
        }
    }

    fn risk(&self, _args: &Value) -> Risk {
        Risk::Observe
    }

    fn run(&self, _args: &Value) -> Result<String, ToolError> {
        let snapshot = uos_hw::probe().map_err(|e| ToolError::Failed(e.to_string()))?;
        serde_json::to_string(&snapshot).map_err(|e| ToolError::Failed(e.to_string()))
    }
}

struct LaunchApp {
    paths: Paths,
}

impl Tool for LaunchApp {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "launch_app",
            description: "Run a Linux or Windows program file inside its own isolated virtual \
                          machine and return what it prints. The program can read the other \
                          files in its folder but cannot change them.",
            parameters: object_schema(
                json!({
                    "path": { "type": "string", "description": "Program file on the host" },
                    "args": { "type": "array", "items": { "type": "string" } },
                }),
                &["path"],
            ),
        }
    }

    /// The program's folder is readable inside the VM and what it prints
    /// comes back to the model, so a folder of settings or credentials asks.
    fn risk(&self, args: &Value) -> Risk {
        let resolved = text_arg(args, "path")
            .ok()
            .and_then(|path| Path::new(path).canonicalize().ok());
        match resolved {
            Some(path) if !is_sensitive_path(&path.to_string_lossy()) => Risk::Modify,
            Some(_) | None => Risk::Privileged,
        }
    }

    fn run(&self, args: &Value) -> Result<String, ToolError> {
        let path = text_arg(args, "path")?;
        let app_args: Vec<String> = match args.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|_| ToolError::BadArgument("args"))?,
        };
        let request = LaunchRequest {
            app: Path::new(path),
            args: &app_args,
            memory_mib: APP_MEMORY_MIB,
            cpus: APP_CPUS,
            interactive: false,
            timeout: Some(APP_TIMEOUT),
            writable_share: false,
        };
        let mut printed = CappedBuffer::default();
        match uos_vm::launch(&request, &self.paths, &mut printed) {
            Ok(outcome) => Ok(format!("exit code: {}\n{}", outcome.exit_code, printed.text())),
            Err(error) => Err(ToolError::Failed(format!(
                "{error}\noutput before it stopped:\n{}",
                printed.text()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("uos-tools-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::create_dir_all(home.join("docs")).unwrap();
        fs::create_dir_all(home.join("bin")).unwrap();
        home.canonicalize().unwrap()
    }

    fn writer(home: &Path) -> WriteFile {
        WriteFile {
            home: Some(home.to_path_buf()),
        }
    }

    #[test]
    fn runs_a_command_and_reports_its_output_and_status() {
        let result = RunCommand
            .run(&json!({ "command": "echo hello; echo oops >&2; exit 3" }))
            .unwrap();

        assert!(result.contains("hello"), "{result}");
        assert!(result.contains("oops"), "{result}");
        assert!(result.contains("exit status: 3"), "{result}");
    }

    #[test]
    fn gives_a_command_no_keyboard_input() {
        let result = RunCommand.run(&json!({ "command": "cat" })).unwrap();

        assert!(result.starts_with("exit status: 0"), "{result}");
    }

    #[test]
    fn keeps_only_the_start_of_endless_command_output() {
        let result = RunCommand
            .run(&json!({ "command": "head -c 5000000 /dev/zero" }))
            .unwrap();

        assert!(result.len() < 2 * MAX_OUTPUT_BYTES, "{}", result.len());
        assert!(result.contains("[truncated: "), "{}", &result[result.len() - 60..]);
    }

    #[test]
    fn rates_a_command_from_its_text() {
        assert_eq!(RunCommand.risk(&json!({ "command": "ls" })), Risk::Observe);
        assert_eq!(RunCommand.risk(&json!({ "command": "touch x" })), Risk::Privileged);
        assert_eq!(RunCommand.risk(&json!({})), Risk::Privileged);
    }

    #[test]
    fn rejects_a_call_without_its_required_argument() {
        let result = RunCommand.run(&json!({ "cmd": "ls" }));

        assert!(matches!(result, Err(ToolError::BadArgument("command"))));
    }

    #[test]
    fn writes_then_reads_a_file() {
        let home = temp_home("roundtrip");
        let path = home.join("docs/note.txt");
        let path_text = path.to_str().unwrap();

        writer(&home)
            .run(&json!({ "path": path_text, "content": "remember" }))
            .unwrap();
        let read = ReadFile.run(&json!({ "path": path_text })).unwrap();

        assert_eq!(read, "remember");
    }

    #[test]
    fn reports_an_unreadable_file() {
        let result = ReadFile.run(&json!({ "path": "/nonexistent/uos" }));

        assert!(matches!(result, Err(ToolError::Failed(_))));
    }

    #[test]
    fn refuses_to_read_a_device() {
        let result = ReadFile.run(&json!({ "path": "/dev/zero" }));

        assert!(matches!(result, Err(ToolError::Failed(m)) if m.contains("not a regular file")));
    }

    #[test]
    fn keeps_only_the_start_of_a_large_file() {
        let home = temp_home("large");
        let path = home.join("docs/big.txt");
        fs::write(&path, vec![b'a'; MAX_OUTPUT_BYTES + 100]).unwrap();

        let read = ReadFile.run(&json!({ "path": path })).unwrap();

        assert!(read.ends_with("[truncated: 100 more bytes]"), "{}", &read[read.len() - 40..]);
    }

    #[test]
    fn reads_ordinary_files_without_asking() {
        assert_eq!(ReadFile.risk(&json!({ "path": "/etc/os-release" })), Risk::Observe);
    }

    #[test]
    fn asks_before_reading_hidden_or_unresolvable_files() {
        let home = temp_home("readrisk");
        fs::write(home.join(".ssh/key"), "secret").unwrap();
        let through_link = home.join("docs/innocent");
        std::os::unix::fs::symlink(home.join(".ssh/key"), &through_link).unwrap();

        assert_eq!(ReadFile.risk(&json!({ "path": home.join(".ssh/key") })), Risk::Privileged);
        assert_eq!(ReadFile.risk(&json!({ "path": through_link })), Risk::Privileged);
        assert_eq!(ReadFile.risk(&json!({ "path": "/nonexistent/x" })), Risk::Privileged);
        assert_eq!(ReadFile.risk(&json!({})), Risk::Privileged);
    }

    #[test]
    fn rates_a_write_inside_home_as_modify() {
        let home = temp_home("inside");

        let risk = writer(&home).risk(&json!({ "path": home.join("docs/a.txt") }));

        assert_eq!(risk, Risk::Modify);
    }

    #[test]
    fn rates_a_write_outside_home_as_privileged() {
        let home = temp_home("outside");

        assert_eq!(writer(&home).risk(&json!({ "path": "/etc/hosts" })), Risk::Privileged);
    }

    #[test]
    fn rates_a_write_to_any_hidden_entry_or_the_bin_folder_as_privileged() {
        let home = temp_home("sensitive");
        let tool = writer(&home);

        for entry in [".bashrc", ".zshenv", ".gitconfig", ".ssh/authorized_keys", "bin/ls"] {
            assert_eq!(tool.risk(&json!({ "path": home.join(entry) })), Risk::Privileged, "{entry}");
        }
        fs::create_dir_all(home.join("docs/.git")).unwrap();
        assert_eq!(
            tool.risk(&json!({ "path": home.join("docs/.git/config") })),
            Risk::Privileged
        );
    }

    #[test]
    fn rates_a_write_that_escapes_home_through_dot_dot_as_privileged() {
        let home = temp_home("dotdot");
        let escaping = home.join("docs/../../escaped.txt");

        assert_eq!(writer(&home).risk(&json!({ "path": escaping })), Risk::Privileged);
    }

    #[test]
    fn rates_a_write_through_a_symlink_as_privileged() {
        let home = temp_home("symlink");
        let link = home.join("docs/link.txt");
        std::os::unix::fs::symlink("/etc/hosts", &link).unwrap();

        assert_eq!(writer(&home).risk(&json!({ "path": link })), Risk::Privileged);
    }

    #[test]
    fn rates_a_write_through_a_hard_link_as_privileged() {
        let home = temp_home("hardlink");
        fs::write(home.join(".ssh/config"), "x").unwrap();
        let link = home.join("docs/alias.txt");
        fs::hard_link(home.join(".ssh/config"), &link).unwrap();

        assert_eq!(writer(&home).risk(&json!({ "path": link })), Risk::Privileged);
    }

    #[test]
    fn rates_a_write_as_privileged_when_home_is_unknown() {
        let tool = WriteFile { home: None };

        assert_eq!(tool.risk(&json!({ "path": "/tmp/x" })), Risk::Privileged);
    }

    #[test]
    fn caps_what_an_application_can_hand_back() {
        let mut buffer = CappedBuffer::default();

        buffer.write_all(&vec![b'a'; MAX_OUTPUT_BYTES - 1]).unwrap();
        buffer.write_all(b"bcd").unwrap();
        buffer.write_all(b"e").unwrap();

        assert_eq!(buffer.kept.len(), MAX_OUTPUT_BYTES);
        assert!(buffer.text().ends_with("b\n[truncated: 3 more bytes]"));
    }

    #[test]
    fn asks_before_launching_a_program_from_a_hidden_folder() {
        let home = temp_home("launchrisk");
        fs::write(home.join(".ssh/tool"), "#!/bin/sh\n").unwrap();
        fs::write(home.join("docs/tool"), "#!/bin/sh\n").unwrap();
        let tool = LaunchApp {
            paths: Paths {
                kernel: "/k".into(),
                image_dir: "/i".into(),
                kernel_release: "r".into(),
            },
        };

        assert_eq!(tool.risk(&json!({ "path": home.join("docs/tool") })), Risk::Modify);
        assert_eq!(tool.risk(&json!({ "path": home.join(".ssh/tool") })), Risk::Privileged);
        assert_eq!(tool.risk(&json!({ "path": "/nonexistent/tool" })), Risk::Privileged);
    }

    #[test]
    fn offers_every_tool_with_a_unique_name() {
        let paths = Paths {
            kernel: "/k".into(),
            image_dir: "/i".into(),
            kernel_release: "r".into(),
        };

        let mut names: Vec<_> = default_tools(paths).iter().map(|t| t.spec().name).collect();
        names.sort_unstable();

        assert_eq!(
            names,
            ["hardware_status", "launch_app", "read_file", "run_command", "write_file"]
        );
    }

    #[test]
    fn reports_a_launch_failure_as_a_tool_error() {
        let tool = LaunchApp {
            paths: Paths {
                kernel: "/k".into(),
                image_dir: "/nonexistent".into(),
                kernel_release: "r".into(),
            },
        };

        let result = tool.run(&json!({ "path": "/bin/ls" }));

        assert!(matches!(result, Err(ToolError::Failed(message)) if message.contains("build-image")));
    }
}
