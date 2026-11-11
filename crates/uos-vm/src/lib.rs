//! Runs each application inside its own virtual machine.

mod detect;
mod launch;
mod spec;

use std::path::PathBuf;

pub use detect::{detect, detect_bytes, is_windows_gui, AppKind, Guest};
pub use launch::{launch, LaunchOutcome, LaunchRequest, Paths, WINDOW_TITLE_PREFIX};
pub use spec::{encode_argv, Display, VmSpec, GUEST_APP_DIR};

#[derive(Debug, thiserror::Error)]
pub enum VmError {
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: std::io::Error,
    },
    #[error("{0} is not a Linux or Windows program UnifiedOS can run yet")]
    Unsupported(PathBuf),
    #[error("guest image {image} is missing; build it with: guest/build-image.sh {guest}")]
    ImageMissing { image: PathBuf, guest: &'static str },
    #[error(
        "guest image {image} was built for kernel {built_for} but the host runs {running}; \
         rebuild it with: guest/build-image.sh {guest}"
    )]
    KernelMismatch {
        image: PathBuf,
        built_for: String,
        running: String,
        guest: &'static str,
    },
    #[error("program arguments must not contain line breaks or NUL bytes")]
    ArgumentHasNewline,
    #[error("program arguments are too long to pass to the guest ({len} > {max} bytes)")]
    ArgumentsTooLong { len: usize, max: usize },
    #[error("{0} was not found; install it to run applications")]
    MissingHostTool(&'static str),
    #[error("the file-sharing helper did not start: {0}")]
    ShareNotReady(String),
    #[error("the virtual machine stopped without reporting an exit code: {0}")]
    NoExitCode(String),
    #[error("the application's window was closed before it exited")]
    WindowClosed,
    #[error("the application was stopped after running for {0} seconds")]
    TimedOut(u64),
    #[error(
        "running {app} would share {dir} with it, which holds your whole home directory; \
         move the program into a folder of its own"
    )]
    ShareTooBroad { app: PathBuf, dir: PathBuf },
    #[error("{0} exists but is not a directory owned by you")]
    UnsafeRunDir(PathBuf),
}

impl VmError {
    pub(crate) fn io(context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }
}
