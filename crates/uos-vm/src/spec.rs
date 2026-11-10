use std::path::PathBuf;

use crate::VmError;

/// Where the application's directory appears inside the guest.
pub const GUEST_APP_DIR: &str = "/app";
/// virtiofs tag the guest mounts at [`GUEST_APP_DIR`].
const SHARE_TAG: &str = "app";
const GUEST_INIT: &str = "/usr/local/bin/uos-init";
/// The x86 kernel command line is capped at 2048 bytes; leave room for the
/// fixed options.
const MAX_ENCODED_ARGV: usize = 1800;

/// Everything needed to start one application's virtual machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmSpec {
    pub kernel: PathBuf,
    pub image: PathBuf,
    /// vhost-user socket of the file-sharing helper.
    pub share_socket: PathBuf,
    /// Host file that receives what the guest init writes to its control port.
    pub control_log: PathBuf,
    pub memory_mib: u32,
    pub cpus: u32,
    /// Command line to run inside the guest.
    pub guest_argv: Vec<String>,
}

impl VmSpec {
    pub fn qemu_args(&self) -> Result<Vec<String>, VmError> {
        let memory = format!("{}M", self.memory_mib);
        let append = format!(
            "console=ttyS1 root=/dev/vda rw quiet loglevel=0 panic=-1 init={GUEST_INIT} uos.argv={}",
            encode_argv(&self.guest_argv)?
        );
        let args = [
            "-nodefaults",
            "-no-user-config",
            "-no-reboot",
            "-display",
            "none",
            "-machine",
            "q35,accel=kvm",
            "-cpu",
            "host",
            "-smp",
            &self.cpus.to_string(),
            "-m",
            &memory,
            // virtiofs needs guest memory the helper process can map.
            "-object",
            &format!("memory-backend-memfd,id=mem,size={memory},share=on"),
            "-numa",
            "node,memdev=mem",
            "-kernel",
            &self.kernel.to_string_lossy(),
            "-append",
            &append,
            // snapshot=on keeps the shared base image untouched.
            "-drive",
            &format!(
                "file={},format=raw,if=virtio,snapshot=on",
                escape_drive_path(&self.image.to_string_lossy())
            ),
            "-chardev",
            &format!(
                "socket,id=share,path={}",
                escape_drive_path(&self.share_socket.to_string_lossy())
            ),
            "-device",
            &format!("vhost-user-fs-pci,chardev=share,tag={SHARE_TAG}"),
            // First serial port: the application's input and output.
            "-serial",
            "stdio",
            // Second serial port: the kernel console and the guest init's
            // exit report. The application runs unprivileged and cannot
            // open it.
            "-chardev",
            &format!(
                "file,id=control,path={}",
                escape_drive_path(&self.control_log.to_string_lossy())
            ),
            "-serial",
            "chardev:control",
        ]
        .map(str::to_string)
        .to_vec();
        Ok(args)
    }
}

/// QEMU option values treat a comma as a separator; a doubled comma is literal.
fn escape_drive_path(path: &str) -> String {
    path.replace(',', ",,")
}

/// Hex-encodes the arguments, one per line, so they survive the kernel
/// command line unchanged.
pub fn encode_argv(argv: &[String]) -> Result<String, VmError> {
    if argv.iter().any(|arg| arg.contains(['\n', '\0'])) {
        return Err(VmError::ArgumentHasNewline);
    }
    let encoded: String = argv
        .join("\n")
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if encoded.len() > MAX_ENCODED_ARGV {
        return Err(VmError::ArgumentsTooLong {
            len: encoded.len(),
            max: MAX_ENCODED_ARGV,
        });
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(argv: &[&str]) -> VmSpec {
        VmSpec {
            kernel: "/boot/vmlinuz".into(),
            image: "/images/linux.ext4".into(),
            share_socket: "/run/uos/fs.sock".into(),
            control_log: "/run/uos/control".into(),
            memory_mib: 512,
            cpus: 2,
            guest_argv: argv.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> &'a str {
        let at = args.iter().position(|a| a == flag).unwrap();
        &args[at + 1]
    }

    #[test]
    fn encodes_arguments_as_newline_separated_hex() {
        let encoded = encode_argv(&["ab".to_string(), "c".to_string()]).unwrap();

        assert_eq!(encoded, "61620a63");
    }

    #[test]
    fn rejects_an_argument_containing_a_line_break() {
        let result = encode_argv(&["a\nb".to_string()]);

        assert!(matches!(result, Err(VmError::ArgumentHasNewline)));
    }

    #[test]
    fn rejects_an_argument_containing_a_nul_byte() {
        let result = encode_argv(&["a\0b".to_string()]);

        assert!(matches!(result, Err(VmError::ArgumentHasNewline)));
    }

    #[test]
    fn rejects_arguments_that_overflow_the_kernel_command_line() {
        let result = encode_argv(&["x".repeat(1000)]);

        assert!(matches!(result, Err(VmError::ArgumentsTooLong { .. })));
    }

    #[test]
    fn boots_the_kernel_directly_with_the_encoded_command() {
        let args = spec(&["/app/hello"]).qemu_args().unwrap();

        assert_eq!(value_after(&args, "-kernel"), "/boot/vmlinuz");
        let append = value_after(&args, "-append");
        assert!(append.contains("init=/usr/local/bin/uos-init"));
        assert!(append.ends_with("uos.argv=2f6170702f68656c6c6f"));
    }

    #[test]
    fn mounts_the_base_image_as_a_throwaway_snapshot() {
        let args = spec(&["/app/hello"]).qemu_args().unwrap();

        assert_eq!(
            value_after(&args, "-drive"),
            "file=/images/linux.ext4,format=raw,if=virtio,snapshot=on"
        );
    }

    #[test]
    fn shares_memory_with_the_file_sharing_helper() {
        let args = spec(&["/app/hello"]).qemu_args().unwrap();

        assert_eq!(value_after(&args, "-m"), "512M");
        assert_eq!(
            value_after(&args, "-object"),
            "memory-backend-memfd,id=mem,size=512M,share=on"
        );
        assert_eq!(value_after(&args, "-chardev"), "socket,id=share,path=/run/uos/fs.sock");
    }

    #[test]
    fn reports_the_exit_code_on_a_separate_port() {
        let args = spec(&["/app/hello"]).qemu_args().unwrap();

        let serials: Vec<_> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| *flag == "-serial")
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(serials, ["stdio", "chardev:control"]);
        assert!(args.contains(&"file,id=control,path=/run/uos/control".to_string()));
    }

    #[test]
    fn gives_the_guest_no_network_device() {
        let args = spec(&["/app/hello"]).qemu_args().unwrap();

        assert!(args.contains(&"-nodefaults".to_string()));
        assert!(!args.iter().any(|a| a.contains("net")));
    }

    #[test]
    fn escapes_commas_in_host_paths() {
        let with_comma = VmSpec {
            image: "/images/a,b.ext4".into(),
            ..spec(&["/app/hello"])
        };

        let args = with_comma.qemu_args().unwrap();

        assert!(value_after(&args, "-drive").starts_with("file=/images/a,,b.ext4,"));
    }
}
