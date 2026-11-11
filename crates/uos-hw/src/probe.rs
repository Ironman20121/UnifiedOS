use std::process::Command;

use serde::Serialize;

use crate::HwError;

const KIB_PER_MIB: u64 = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Gpu {
    pub name: String,
    pub total_mib: u64,
    pub used_mib: u64,
}

impl Gpu {
    pub fn free_mib(&self) -> u64 {
        self.total_mib.saturating_sub(self.used_mib)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HardwareSnapshot {
    pub gpus: Vec<Gpu>,
    pub ram_total_mib: u64,
    pub ram_available_mib: u64,
    pub cpu_cores: usize,
}

impl HardwareSnapshot {
    pub fn vram_free_mib(&self) -> u64 {
        self.gpus.iter().map(Gpu::free_mib).sum()
    }
}

/// Parses `nvidia-smi --query-gpu=name,memory.total,memory.used
/// --format=csv,noheader,nounits`.
pub fn parse_nvidia_smi(csv: &str) -> Result<Vec<Gpu>, HwError> {
    csv.lines()
        .filter(|line| !line.trim().is_empty())
        .map(parse_gpu_line)
        .collect()
}

fn parse_gpu_line(line: &str) -> Result<Gpu, HwError> {
    let bad = || HwError::Parse {
        what: "nvidia-smi",
        detail: line.to_string(),
    };
    // The name can itself contain commas, so take the numbers from the right.
    let mut fields = line.rsplitn(3, ',').map(str::trim);
    let used_mib = fields.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
    let total_mib = fields.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
    let name = fields.next().ok_or_else(bad)?.to_string();
    Ok(Gpu {
        name,
        total_mib,
        used_mib,
    })
}

/// Parses `/proc/meminfo` into `(total, available)` in MiB.
pub fn parse_meminfo(text: &str) -> Result<(u64, u64), HwError> {
    let field = |key: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kib| kib.parse::<u64>().ok())
            .map(|kib| kib / KIB_PER_MIB)
            .ok_or_else(|| HwError::Parse {
                what: "/proc/meminfo",
                detail: format!("missing {key}"),
            })
    };
    Ok((field("MemTotal:")?, field("MemAvailable:")?))
}

/// Reads the live state of the machine. A machine without an NVIDIA GPU is
/// reported with an empty GPU list, not as an error.
pub fn probe() -> Result<HardwareSnapshot, HwError> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").map_err(|source| HwError::Io {
        what: "/proc/meminfo",
        source,
    })?;
    let (ram_total_mib, ram_available_mib) = parse_meminfo(&meminfo)?;
    let cpu_cores = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    Ok(HardwareSnapshot {
        gpus: probe_gpus()?,
        ram_total_mib,
        ram_available_mib,
        cpu_cores,
    })
}

fn probe_gpus() -> Result<Vec<Gpu>, HwError> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=name,memory.total,memory.used",
            "--format=csv,noheader,nounits",
        ])
        .output();
    match output {
        Ok(out) if out.status.success() => parse_nvidia_smi(&String::from_utf8_lossy(&out.stdout)),
        // A GPU that is present but failing must not silently become "no GPU":
        // the model would be forced onto the CPU without anyone noticing.
        Ok(out) => Err(HwError::Gpu(format!(
            "nvidia-smi: {} {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
        // No NVIDIA tooling installed: a CPU-only machine.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(HwError::Io {
            what: "nvidia-smi",
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_single_gpu_line() {
        let gpus = parse_nvidia_smi("Tesla PG500-216, 32768, 512\n").unwrap();

        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].name, "Tesla PG500-216");
        assert_eq!(gpus[0].free_mib(), 32256);
    }

    #[test]
    fn keeps_commas_inside_the_gpu_name() {
        let gpus = parse_nvidia_smi("Vendor, Model X, 8192, 100").unwrap();

        assert_eq!(gpus[0].name, "Vendor, Model X");
        assert_eq!(gpus[0].total_mib, 8192);
    }

    #[test]
    fn rejects_a_line_without_numbers() {
        assert!(parse_nvidia_smi("Tesla, [N/A], [N/A]").is_err());
    }

    #[test]
    fn returns_no_gpus_for_empty_output() {
        assert!(parse_nvidia_smi("\n").unwrap().is_empty());
    }

    #[test]
    fn parses_meminfo_into_mebibytes() {
        let text = "MemTotal:       32000000 kB\nMemFree: 1 kB\nMemAvailable:   16384000 kB\n";

        assert_eq!(parse_meminfo(text).unwrap(), (31250, 16000));
    }

    #[test]
    fn reports_a_missing_meminfo_field() {
        assert!(parse_meminfo("MemTotal: 1024 kB\n").is_err());
    }

    #[test]
    fn sums_free_vram_across_gpus() {
        let gpu = |used| Gpu {
            name: "g".into(),
            total_mib: 1000,
            used_mib: used,
        };
        let snapshot = HardwareSnapshot {
            gpus: vec![gpu(100), gpu(400)],
            ram_total_mib: 0,
            ram_available_mib: 0,
            cpu_cores: 1,
        };

        assert_eq!(snapshot.vram_free_mib(), 1500);
    }
}
