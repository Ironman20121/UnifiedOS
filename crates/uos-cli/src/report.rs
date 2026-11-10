use std::fmt::Write;

use uos_hw::{HardwareSnapshot, ModelProfile, Tier, TierPlan};

pub fn hardware(snapshot: &HardwareSnapshot) -> String {
    let mut text = String::new();
    if snapshot.gpus.is_empty() {
        text.push_str("GPU   none detected\n");
    }
    for gpu in &snapshot.gpus {
        let _ = writeln!(
            text,
            "GPU   {}: {} MiB free of {} MiB",
            gpu.name,
            gpu.free_mib(),
            gpu.total_mib
        );
    }
    let _ = writeln!(
        text,
        "RAM   {} MiB available of {} MiB",
        snapshot.ram_available_mib, snapshot.ram_total_mib
    );
    let _ = writeln!(text, "CPU   {} cores", snapshot.cpu_cores);
    text
}

pub fn placement(model: &ModelProfile, plan: &TierPlan) -> String {
    let summary = match plan.tier {
        Tier::Gpu => "fully on the GPU",
        Tier::GpuAndRam => "GPU is full; the rest runs from RAM",
        Tier::GpuRamAndDisk => "GPU and RAM are full; the rest is paged from disk",
    };
    format!(
        "Model {} ({} MiB with context, {} layers): {summary}\n  GPU   {} of {} layers, {} MiB\n  RAM   {} MiB\n  Disk  {} MiB\n",
        model.name,
        plan.footprint_mib,
        plan.total_layers,
        plan.gpu_layers,
        plan.total_layers,
        plan.vram_mib,
        plan.ram_mib,
        plan.disk_mib,
    )
}

#[cfg(test)]
mod tests {
    use uos_hw::Gpu;

    use super::*;

    #[test]
    fn lists_each_gpu_with_its_free_memory() {
        let snapshot = HardwareSnapshot {
            gpus: vec![Gpu {
                name: "Tesla".into(),
                total_mib: 32768,
                used_mib: 768,
            }],
            ram_total_mib: 31000,
            ram_available_mib: 20000,
            cpu_cores: 18,
        };

        let text = hardware(&snapshot);

        assert!(text.contains("Tesla: 32000 MiB free of 32768 MiB"), "{text}");
        assert!(text.contains("RAM   20000 MiB available of 31000 MiB"), "{text}");
        assert!(text.contains("CPU   18 cores"), "{text}");
    }

    #[test]
    fn says_when_there_is_no_gpu() {
        let snapshot = HardwareSnapshot {
            gpus: Vec::new(),
            ram_total_mib: 1,
            ram_available_mib: 1,
            cpu_cores: 1,
        };

        assert!(hardware(&snapshot).starts_with("GPU   none detected\n"));
    }

    #[test]
    fn describes_a_model_split_across_gpu_and_ram() {
        let model = ModelProfile {
            name: "big".into(),
            weights_mib: 1000,
            layers: 10,
            cache_bytes_per_token: 0,
        };
        let plan = TierPlan {
            tier: Tier::GpuAndRam,
            footprint_mib: 1000,
            gpu_layers: 4,
            total_layers: 10,
            vram_mib: 400,
            ram_mib: 600,
            disk_mib: 0,
        };

        let text = placement(&model, &plan);

        assert!(text.contains("GPU is full; the rest runs from RAM"), "{text}");
        assert!(text.contains("GPU   4 of 10 layers, 400 MiB"), "{text}");
        assert!(text.contains("RAM   600 MiB"), "{text}");
    }
}
