//! Hardware probing and GPU-first model placement.

mod plan;
mod probe;

pub use plan::{plan, ModelProfile, PlanOptions, Reserve, Tier, TierPlan};
pub use probe::{parse_meminfo, parse_nvidia_smi, probe, Gpu, HardwareSnapshot};

#[derive(Debug, thiserror::Error)]
pub enum HwError {
    #[error("could not read {what}: {source}")]
    Io {
        what: &'static str,
        source: std::io::Error,
    },
    #[error("the GPU could not be queried: {0}")]
    Gpu(String),
    #[error("unexpected {what} output: {detail}")]
    Parse { what: &'static str, detail: String },
}
