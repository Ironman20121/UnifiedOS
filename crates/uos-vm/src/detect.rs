use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::VmError;

const ELF_MAGIC: &[u8] = b"\x7fELF";
const PE_MAGIC: &[u8] = b"MZ";
const SCRIPT_MAGIC: &[u8] = b"#!";
/// Enough of the file to cover the headers of any ordinary executable.
const HEADER_LEN: usize = 4096;
/// Offset of the field holding the PE header's position.
const PE_OFFSET_FIELD: usize = 0x3C;
const PE_SIGNATURE: &[u8] = b"PE\0\0";
/// Offset of the subsystem field from the start of the PE header: the
/// signature, the 20-byte COFF header, then 68 bytes into the optional header.
const PE_SUBSYSTEM_OFFSET: usize = 4 + 20 + 68;
const PE_SUBSYSTEM_GUI: u16 = 2;

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

/// Whether a Windows executable declares itself a graphical program.
pub fn is_windows_gui(header: &[u8]) -> bool {
    let u16_at = |at: usize| Some(u16::from_le_bytes(header.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(header.get(at..at + 4)?.try_into().ok()?));
    let subsystem = || {
        let pe_header = usize::try_from(u32_at(PE_OFFSET_FIELD)?).ok()?;
        let signature = header.get(pe_header..pe_header.checked_add(PE_SIGNATURE.len())?)?;
        if signature != PE_SIGNATURE {
            return None;
        }
        u16_at(pe_header.checked_add(PE_SUBSYSTEM_OFFSET)?)
    };
    header.starts_with(PE_MAGIC) && subsystem() == Some(PE_SUBSYSTEM_GUI)
}

/// What kind of application a file is, and whether it asks for a window.
pub fn detect(path: &Path) -> Result<(AppKind, bool), VmError> {
    let mut header = Vec::new();
    File::open(path)
        .and_then(|file| file.take(HEADER_LEN as u64).read_to_end(&mut header))
        .map_err(VmError::io(format!("could not read {}", path.display())))?;
    let kind = detect_bytes(&header).ok_or_else(|| VmError::Unsupported(path.to_path_buf()))?;
    Ok((kind, is_windows_gui(&header)))
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

        assert_eq!(detect(&me).unwrap(), (AppKind::LinuxElf, false));
    }

    /// A PE image with its header at `pe_header` and the given subsystem.
    fn pe_image(pe_header: usize, subsystem: u16) -> Vec<u8> {
        let mut image = vec![0u8; pe_header + PE_SUBSYSTEM_OFFSET + 2];
        image[..2].copy_from_slice(PE_MAGIC);
        image[PE_OFFSET_FIELD..PE_OFFSET_FIELD + 4].copy_from_slice(&(pe_header as u32).to_le_bytes());
        image[pe_header..pe_header + 4].copy_from_slice(PE_SIGNATURE);
        let at = pe_header + PE_SUBSYSTEM_OFFSET;
        image[at..at + 2].copy_from_slice(&subsystem.to_le_bytes());
        image
    }

    #[test]
    fn tells_graphical_windows_programs_from_console_ones() {
        assert!(is_windows_gui(&pe_image(0x80, 2)));
        assert!(!is_windows_gui(&pe_image(0x80, 3)));
    }

    #[test]
    fn treats_truncated_or_malformed_windows_headers_as_console() {
        let gui = pe_image(0x80, 2);
        let mut bad_signature = gui.clone();
        bad_signature[0x80] = b'X';
        let mut far_header = gui.clone();
        far_header[PE_OFFSET_FIELD..PE_OFFSET_FIELD + 4].copy_from_slice(&u32::MAX.to_le_bytes());

        assert!(!is_windows_gui(&gui[..gui.len() - 1]));
        assert!(!is_windows_gui(&bad_signature));
        assert!(!is_windows_gui(&far_header));
        assert!(!is_windows_gui(b"MZ"));
        assert!(!is_windows_gui(b"\x7fELF"));
    }

    #[test]
    fn reads_the_console_subsystem_of_the_test_fixture() {
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/exit42.exe");

        assert_eq!(detect(Path::new(fixture)).unwrap(), (AppKind::WindowsPe, false));
    }

    #[test]
    fn reads_the_graphical_subsystem_of_the_test_fixture() {
        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/exit42-gui.exe");

        assert_eq!(detect(Path::new(fixture)).unwrap(), (AppKind::WindowsPe, true));
    }
}
