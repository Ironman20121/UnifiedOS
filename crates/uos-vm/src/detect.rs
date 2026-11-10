use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::VmError;

const ELF_MAGIC: &[u8] = b"\x7fELF";
const PE_MAGIC: &[u8] = b"MZ";
const SCRIPT_MAGIC: &[u8] = b"#!";
const HEADER_LEN: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppKind {
    LinuxElf,
    Script,
    WindowsPe,
}

/// The guest image an application runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Guest {
    Linux,
    /// A Linux guest carrying Wine.
    Windows,
}

impl Guest {
    pub fn name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }
}

impl AppKind {
    pub fn guest(self) -> Guest {
        match self {
            Self::LinuxElf | Self::Script => Guest::Linux,
            Self::WindowsPe => Guest::Windows,
        }
    }
}

/// Identifies an application from the first bytes of its file.
pub fn detect_bytes(header: &[u8]) -> Option<AppKind> {
    if header.starts_with(ELF_MAGIC) {
        Some(AppKind::LinuxElf)
    } else if header.starts_with(PE_MAGIC) {
        Some(AppKind::WindowsPe)
    } else if header.starts_with(SCRIPT_MAGIC) {
        Some(AppKind::Script)
    } else {
        None
    }
}

pub fn detect(path: &Path) -> Result<AppKind, VmError> {
    let mut header = [0u8; HEADER_LEN];
    let read = File::open(path)
        .and_then(|mut file| file.read(&mut header))
        .map_err(VmError::io(format!("could not read {}", path.display())))?;
    detect_bytes(&header[..read]).ok_or_else(|| VmError::Unsupported(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_linux_executable() {
        assert_eq!(detect_bytes(b"\x7fELF\x02\x01"), Some(AppKind::LinuxElf));
    }

    #[test]
    fn recognises_a_windows_executable() {
        assert_eq!(detect_bytes(b"MZ\x90\x00"), Some(AppKind::WindowsPe));
    }

    #[test]
    fn recognises_a_script() {
        assert_eq!(detect_bytes(b"#!/bin/sh\n"), Some(AppKind::Script));
    }

    #[test]
    fn rejects_unknown_and_empty_files() {
        assert_eq!(detect_bytes(b"PK\x03\x04"), None);
        assert_eq!(detect_bytes(b""), None);
    }

    #[test]
    fn routes_windows_programs_to_the_windows_guest() {
        assert_eq!(AppKind::WindowsPe.guest(), Guest::Windows);
        assert_eq!(AppKind::LinuxElf.guest(), Guest::Linux);
        assert_eq!(AppKind::Script.guest(), Guest::Linux);
    }

    #[test]
    fn reports_a_missing_file() {
        let result = detect(Path::new("/nonexistent/uos-test-app"));

        assert!(matches!(result, Err(VmError::Io { .. })));
    }

    #[test]
    fn detects_the_running_test_binary_as_linux() {
        let me = std::env::current_exe().unwrap();

        assert_eq!(detect(&me).unwrap(), AppKind::LinuxElf);
    }
}
