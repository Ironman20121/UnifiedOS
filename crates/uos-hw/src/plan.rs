use serde::Serialize;

use crate::HardwareSnapshot;

const BYTES_PER_MIB: u64 = 1024 * 1024;

/// What the planner needs to know about a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelProfile {
    pub name: String,
    pub weights_mib: u64,
    /// Number of layers the model server can offload individually.
    pub layers: u32,
    /// Attention cache the model keeps for every token of context.
    pub cache_bytes_per_token: u64,
}

impl ModelProfile {
    /// Memory the model occupies once loaded: weights plus context cache.
    pub fn footprint_mib(&self, context_tokens: u32) -> u64 {
        let cache_bytes = self.cache_bytes_per_token * u64::from(context_tokens);
        self.weights_mib + cache_bytes.div_ceil(BYTES_PER_MIB)
    }
}

/// Memory kept free on each tier for everything that is not the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reserve {
    /// Scratch buffers on the GPU.
    pub vram_mib: u64,
    /// Left for the rest of the system.
    pub ram_mib: u64,
}

impl Default for Reserve {
    fn default() -> Self {
        Self {
            vram_mib: 1024,
            ram_mib: 4096,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanOptions {
    pub context_tokens: u32,
    /// VRAM the model server holds for models it will unload to make room.
    pub reclaimable_vram_mib: u64,
    /// Upper bound on the VRAM the model may use, to leave room for other work.
    pub vram_limit_mib: Option<u64>,
    pub reserve: Reserve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Tier {
    /// Every layer is on the GPU.
    Gpu,
    /// The GPU is full; the remaining layers run from RAM on the CPU.
    GpuAndRam,
    /// GPU and RAM are both full; the rest is paged from disk.
    GpuRamAndDisk,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TierPlan {
    pub tier: Tier,
    pub gpu_layers: u32,
    pub total_layers: u32,
    pub footprint_mib: u64,
    pub vram_mib: u64,
    pub ram_mib: u64,
    pub disk_mib: u64,
}

/// Splits a model across VRAM, RAM and disk, filling the fastest tier first.
pub fn plan(model: &ModelProfile, hardware: &HardwareSnapshot, options: &PlanOptions) -> TierPlan {
    let layers = model.layers.max(1);
    let footprint_mib = model.footprint_mib(options.context_tokens);
    let per_layer_mib = footprint_mib.div_ceil(u64::from(layers)).max(1);

    let vram_budget = usable_vram_mib(hardware, options);
    // Per-layer sizes are rounded up, so check the whole model first or one
    // that fits exactly would lose its last layer to rounding.
    let gpu_layers = if vram_budget >= footprint_mib {
        layers
    } else {
        u32::try_from(vram_budget / per_layer_mib).map_or(layers, |fitting| fitting.min(layers))
    };
    let vram_mib = (u64::from(gpu_layers) * per_layer_mib).min(footprint_mib);

    let off_gpu_mib = footprint_mib - vram_mib;
    let ram_budget = hardware
        .ram_available_mib
        .saturating_sub(options.reserve.ram_mib);
    let ram_mib = off_gpu_mib.min(ram_budget);
    let disk_mib = off_gpu_mib - ram_mib;

    let tier = match (off_gpu_mib, disk_mib) {
        (0, _) => Tier::Gpu,
        (_, 0) => Tier::GpuAndRam,
        _ => Tier::GpuRamAndDisk,
    };
    TierPlan {
        tier,
        gpu_layers,
        total_layers: layers,
        footprint_mib,
        vram_mib,
        ram_mib,
        disk_mib,
    }
}

fn usable_vram_mib(hardware: &HardwareSnapshot, options: &PlanOptions) -> u64 {
    let total: u64 = hardware.gpus.iter().map(|gpu| gpu.total_mib).sum();
    let obtainable = (hardware.vram_free_mib() + options.reclaimable_vram_mib).min(total);
    let capped = options
        .vram_limit_mib
        .map_or(obtainable, |limit| obtainable.min(limit));
    capped.saturating_sub(options.reserve.vram_mib)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Gpu;

    const NO_RESERVE: Reserve = Reserve {
        vram_mib: 0,
        ram_mib: 0,
    };

    fn options() -> PlanOptions {
        PlanOptions {
            context_tokens: 0,
            reclaimable_vram_mib: 0,
            vram_limit_mib: None,
            reserve: NO_RESERVE,
        }
    }

    fn hardware(vram_free: u64, ram_available: u64) -> HardwareSnapshot {
        HardwareSnapshot {
            gpus: vec![Gpu {
                name: "test".into(),
                total_mib: vram_free,
                used_mib: 0,
            }],
            ram_total_mib: ram_available,
            ram_available_mib: ram_available,
            cpu_cores: 8,
        }
    }

    fn model(weights_mib: u64, layers: u32) -> ModelProfile {
        ModelProfile {
            name: "m".into(),
            weights_mib,
            layers,
            cache_bytes_per_token: 0,
        }
    }

    #[test]
    fn puts_every_layer_on_the_gpu_when_it_fits() {
        let plan = plan(&model(1000, 10), &hardware(2000, 500), &options());

        assert_eq!(plan.tier, Tier::Gpu);
        assert_eq!(plan.gpu_layers, 10);
        assert_eq!((plan.vram_mib, plan.ram_mib, plan.disk_mib), (1000, 0, 0));
    }

    #[test]
    fn spills_to_ram_when_the_gpu_is_full() {
        let plan = plan(&model(1000, 10), &hardware(450, 4000), &options());

        assert_eq!(plan.tier, Tier::GpuAndRam);
        assert_eq!(plan.gpu_layers, 4);
        assert_eq!((plan.vram_mib, plan.ram_mib, plan.disk_mib), (400, 600, 0));
    }

    #[test]
    fn spills_to_disk_when_ram_is_also_full() {
        let plan = plan(&model(1000, 10), &hardware(300, 200), &options());

        assert_eq!(plan.tier, Tier::GpuRamAndDisk);
        assert_eq!((plan.vram_mib, plan.ram_mib, plan.disk_mib), (300, 200, 500));
    }

    #[test]
    fn holds_back_the_reserve_on_both_tiers() {
        let reserved = PlanOptions {
            reserve: Reserve {
                vram_mib: 200,
                ram_mib: 100,
            },
            ..options()
        };

        let plan = plan(&model(1000, 10), &hardware(500, 300), &reserved);

        assert_eq!((plan.vram_mib, plan.ram_mib, plan.disk_mib), (300, 200, 500));
    }

    #[test]
    fn counts_the_context_cache_towards_the_footprint() {
        let with_cache = ModelProfile {
            cache_bytes_per_token: BYTES_PER_MIB / 2,
            ..model(1000, 10)
        };
        let long_context = PlanOptions {
            context_tokens: 2000,
            ..options()
        };

        let plan = plan(&with_cache, &hardware(1500, 4000), &long_context);

        assert_eq!(plan.footprint_mib, 2000);
        assert_eq!(plan.gpu_layers, 7);
        assert_eq!(plan.tier, Tier::GpuAndRam);
    }

    #[test]
    fn counts_vram_the_model_server_will_free() {
        let busy = HardwareSnapshot {
            gpus: vec![Gpu {
                name: "test".into(),
                total_mib: 2000,
                used_mib: 1800,
            }],
            ..hardware(0, 4000)
        };
        let reclaiming = PlanOptions {
            reclaimable_vram_mib: 1500,
            ..options()
        };

        let plan = plan(&model(1000, 10), &busy, &reclaiming);

        assert_eq!(plan.tier, Tier::Gpu);
    }

    #[test]
    fn never_counts_more_reclaimable_vram_than_the_gpu_has() {
        let reclaiming = PlanOptions {
            reclaimable_vram_mib: 9000,
            ..options()
        };

        let plan = plan(&model(1000, 10), &hardware(500, 4000), &reclaiming);

        assert_eq!(plan.vram_mib, 500);
    }

    #[test]
    fn respects_a_vram_limit() {
        let limited = PlanOptions {
            vram_limit_mib: Some(300),
            ..options()
        };

        let plan = plan(&model(1000, 10), &hardware(2000, 4000), &limited);

        assert_eq!(plan.gpu_layers, 3);
        assert_eq!(plan.tier, Tier::GpuAndRam);
    }

    #[test]
    fn runs_without_a_gpu() {
        let cpu_only = HardwareSnapshot {
            gpus: Vec::new(),
            ..hardware(0, 4000)
        };

        let plan = plan(&model(1000, 10), &cpu_only, &options());

        assert_eq!(plan.gpu_layers, 0);
        assert_eq!(plan.tier, Tier::GpuAndRam);
        assert_eq!(plan.ram_mib, 1000);
    }

    #[test]
    fn never_accounts_for_more_than_the_model_occupies() {
        // 1001 / 10 rounds up to 101 MiB per layer; ten layers must still total 1001.
        let plan = plan(&model(1001, 10), &hardware(5000, 0), &options());

        assert_eq!(plan.vram_mib, 1001);
        assert_eq!(plan.tier, Tier::Gpu);
    }

    #[test]
    fn keeps_a_model_that_fits_exactly_entirely_on_the_gpu() {
        let plan = plan(&model(1001, 10), &hardware(1001, 4000), &options());

        assert_eq!(plan.gpu_layers, 10);
        assert_eq!(plan.tier, Tier::Gpu);
    }

    #[test]
    fn treats_a_model_reporting_zero_layers_as_one_layer() {
        let plan = plan(&model(1000, 0), &hardware(5000, 0), &options());

        assert_eq!(plan.total_layers, 1);
        assert_eq!(plan.gpu_layers, 1);
    }
}
