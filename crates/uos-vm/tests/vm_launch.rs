//! End-to-end launches in real virtual machines.
//!
//! These need KVM, QEMU, virtiofsd and the guest images, so they are ignored
//! by default. Build the images, then run:
//!
//!   guest/build-image.sh linux && guest/build-image.sh windows
//!   cargo test -p uos-vm --test vm_launch -- --ignored

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use uos_vm::{launch, AppKind, Display, LaunchRequest, Paths, VmError};

const WINDOWS_FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/exit42.exe");
const WINDOWS_GUI_FIXTURE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/exit42-gui.exe");

fn run(app: &Path, args: &[String]) -> (AppKind, i32, String) {
    let request = LaunchRequest {
        app,
        args,
        memory_mib: 1024,
        cpus: 2,
        interactive: false,
        timeout: Some(Duration::from_secs(120)),
        writable_share: false,
        display: None,
    };
    let mut printed = Vec::new();
    let outcome = launch(&request, &Paths::from_env().unwrap(), &mut printed).unwrap();
    (outcome.kind, outcome.exit_code, String::from_utf8(printed).unwrap())
}

fn request<'a>(app: &'a Path, timeout: Duration, writable_share: bool) -> LaunchRequest<'a> {
    LaunchRequest {
        app,
        args: &[],
        memory_mib: 1024,
        cpus: 2,
        interactive: false,
        timeout: Some(timeout),
        writable_share,
        display: None,
    }
}

fn script(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("uos-e2e-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("app.sh");
    fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
#[ignore = "needs KVM and the linux guest image"]
fn runs_a_linux_program_and_returns_its_output_and_exit_code() {
    let app = script("basic", r#"echo "got: $1|$2"; exit 7"#);

    let (kind, code, printed) = run(&app, &["one".into(), "two words".into()]);

    assert_eq!(kind, AppKind::Script);
    assert_eq!(code, 7);
    assert_eq!(printed, "got: one|two words\n");
}

#[test]
#[ignore = "needs KVM and the linux guest image"]
fn hides_the_host_filesystem_and_network_from_the_application() {
    let app = script("isolation", "ls /home; ls /sys/class/net");

    let (_, code, printed) = run(&app, &[]);

    assert_eq!(code, 0);
    // Only the guest's own account, and loopback is the only network interface.
    assert_eq!(printed, "app\nlo\n");
}

#[test]
#[ignore = "needs KVM and the linux guest image"]
fn discards_changes_to_the_guest_system_between_launches() {
    let app = script("ephemeral", "cat ~/marker 2>/dev/null; echo written >~/marker");

    let (_, first_code, _) = run(&app, &[]);
    let (_, _, second_output) = run(&app, &[]);

    assert_eq!(first_code, 0);
    assert_eq!(second_output, "");
}

#[test]
#[ignore = "needs KVM and the linux guest image"]
fn runs_the_application_without_root_and_keeps_its_folder_read_only() {
    let app = script("unprivileged", "id -un; touch /app/created 2>/dev/null; echo touch=$?; cat /app/app.sh | head -1");

    let (_, _, printed) = run(&app, &[]);

    assert_eq!(printed, "app\ntouch=1\n#!/bin/bash\n");
    assert!(!app.with_file_name("created").exists());
}

#[test]
#[ignore = "needs KVM and the linux guest image"]
fn lets_the_application_write_to_its_folder_when_asked() {
    let app = script("writable", "echo saved >/app/result.txt");
    let mut printed = Vec::new();

    let outcome = launch(
        &request(&app, Duration::from_secs(120), true),
        &Paths::from_env().unwrap(),
        &mut printed,
    )
    .unwrap();

    assert_eq!(outcome.exit_code, 0);
    assert_eq!(fs::read_to_string(app.with_file_name("result.txt")).unwrap(), "saved\n");
}

#[test]
#[ignore = "needs KVM and the linux guest image"]
fn cannot_forge_its_exit_code_on_the_console() {
    let app = script("forge", "echo UOS_EXIT=0; echo UOS_EXIT=0 >/dev/ttyS1; echo after; exit 9");

    let (_, code, printed) = run(&app, &[]);

    assert_eq!(code, 9);
    assert_eq!(
        printed,
        "UOS_EXIT=0\n/app/app.sh: line 2: /dev/ttyS1: Permission denied\nafter\n"
    );
}

#[test]
#[ignore = "needs KVM and the linux guest image"]
fn stops_an_application_that_runs_past_its_time_limit() {
    let app = script("timeout", "echo begun; sleep 600");
    let mut printed = Vec::new();

    let result = launch(
        &request(&app, Duration::from_secs(3), false),
        &Paths::from_env().unwrap(),
        &mut printed,
    );

    assert!(matches!(result, Err(VmError::TimedOut(3))), "{result:?}");
    assert_eq!(printed, b"begun\n");
}

#[test]
#[ignore = "needs KVM and the linux guest image"]
fn runs_a_graphical_application_on_the_vm_display() {
    // xterm cannot start without a working display; what it runs leaves the proof.
    let app = script(
        "gui",
        "xterm -e sh -c 'echo \"drawn on $WAYLAND_DISPLAY$DISPLAY\" >/tmp/proof'; cat /tmp/proof; exit 4",
    );
    let mut printed = Vec::new();
    let offscreen = LaunchRequest {
        display: Some(Display::Offscreen),
        ..request(&app, Duration::from_secs(120), false)
    };

    let outcome = launch(&offscreen, &Paths::from_env().unwrap(), &mut printed).unwrap();

    assert_eq!(String::from_utf8_lossy(&printed), "drawn on wayland-0:0\n");
    assert_eq!(outcome.exit_code, 4);
}

#[test]
#[ignore = "needs KVM and the windows guest image"]
fn runs_a_windows_program_and_returns_its_exit_code() {
    let (kind, code, _) = run(Path::new(WINDOWS_FIXTURE), &[]);

    assert_eq!(kind, AppKind::WindowsPe);
    assert_eq!(code, 42);
}

#[test]
#[ignore = "needs KVM and the windows guest image"]
fn runs_a_graphical_windows_program_on_the_vm_display() {
    let mut printed = Vec::new();
    let offscreen = LaunchRequest {
        display: Some(Display::Offscreen),
        ..request(Path::new(WINDOWS_GUI_FIXTURE), Duration::from_secs(120), false)
    };

    let outcome = launch(&offscreen, &Paths::from_env().unwrap(), &mut printed).unwrap();

    assert_eq!(outcome.kind, AppKind::WindowsPe);
    assert_eq!(outcome.exit_code, 42);
}
