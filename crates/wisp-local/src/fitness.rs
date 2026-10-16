//! 设备适配：按内存总量、是否 Apple 芯片、显卡显存与「使用 GPU 加速」开关，判断每个模型是否适合本机。
//!
//! | 模型 | 适合 | 较慢 | 内存不足 |
//! |---|---|---|---|
//! | PP-OCRv6 Tiny / Small | 总是 | — | — |
//! | PaddleOCR-VL-1.6 | Apple 芯片且内存 ≥ 8 GB；或显存 ≥ 3 GB | 只有 CPU（几秒到十几秒一页） | 内存 < 6 GB |
//! | Index-Translate 2B | 内存 ≥ 8 GB（CPU 也流畅） | 内存 6–8 GB | 内存 < 6 GB |
//! | Index-Translate 9B | Apple 芯片且内存 ≥ 16 GB；或显存 ≥ 7 GB | 只有 CPU 且内存 ≥ 16 GB（约 4–8 字 / 秒） | 内存 < 12 GB |
//!
//! Windows / Linux 在 llama.cpp 运行时下载之前检测不到显卡：需要显卡的模型报 `unknown`（界面说明下载推理组件后检测）。
//! 集成显卡（[`crate::llama::is_integrated_gpu`]）的「显存」是共享的系统内存，不算显存：只有核显时按 CPU 档评估，
//! 原因写明是核显。「内存不足」仍允许下载（界面二次确认）；加载时另有按可用内存的实时检查。

use wisp_types::{DeviceInfo, Fitness, FitnessLevel, LocalModelId};

/// 下载推理组件后才能检测显卡
pub const REASON_GPU_UNKNOWN: &str = "settings.offline.fitness.gpu_unknown";
/// 没有可用的显卡（或关闭了 GPU 加速），在 CPU 上运行较慢
pub const REASON_CPU_ONLY: &str = "settings.offline.fitness.cpu_only";
/// 内存偏少：能运行但可能较慢，或内存不足
pub const REASON_LOW_MEMORY: &str = "settings.offline.fitness.low_memory";
/// 读不到本机内存
pub const REASON_MEMORY_UNKNOWN: &str = "settings.offline.fitness.memory_unknown";
/// 只有集成显卡（共享系统内存），按 CPU 档评估，运行较慢
pub const REASON_INTEGRATED_GPU: &str = "settings.offline.fitness.integrated_gpu";

const GB: u64 = 1024 * 1024 * 1024;

/// 评估一个模型（纯函数）
pub fn assess(id: LocalModelId, device: &DeviceInfo, use_gpu: bool) -> Fitness {
    if matches!(id, LocalModelId::PpOcrV6Tiny | LocalModelId::PpOcrV6Small) {
        return Fitness::plain(FitnessLevel::Good);
    }
    let Some(total) = device.total_memory else {
        return with_reason(FitnessLevel::Unknown, REASON_MEMORY_UNKNOWN);
    };
    // 只有独立显卡的显存算数；核显的「显存」是划给它的系统内存
    let integrated = |g: &&wisp_types::GpuInfo| crate::llama::is_integrated_gpu(g, Some(total));
    let vram_gb = if use_gpu {
        device
            .gpus
            .iter()
            .filter(|g| !integrated(g))
            .map(|g| u64::from(g.vram_total_mb) * 1024 * 1024)
            .max()
            .unwrap_or(0)
    } else {
        0
    };
    let only_integrated = use_gpu && device.gpus.iter().any(|g| integrated(&g));
    // 需要显卡才算「适合」的模型：Apple 芯片按统一内存判断；其他系统还没检测过显卡时不下结论
    let needs_gpu = |apple_min: u64, vram_min: u64, insufficient_below: u64, cpu_ok_from: u64| {
        if total < insufficient_below {
            return with_reason(FitnessLevel::Insufficient, REASON_LOW_MEMORY);
        }
        if device.apple_silicon && use_gpu {
            return if total >= apple_min {
                Fitness::plain(FitnessLevel::Good)
            } else {
                with_reason(FitnessLevel::Slow, REASON_LOW_MEMORY)
            };
        }
        if vram_gb >= vram_min {
            return Fitness::plain(FitnessLevel::Good);
        }
        if use_gpu && !device.apple_silicon && !device.gpu_probed {
            return with_reason(FitnessLevel::Unknown, REASON_GPU_UNKNOWN);
        }
        if total < cpu_ok_from {
            with_reason(FitnessLevel::Slow, REASON_LOW_MEMORY)
        } else if only_integrated {
            with_reason(FitnessLevel::Slow, REASON_INTEGRATED_GPU)
        } else {
            with_reason(FitnessLevel::Slow, REASON_CPU_ONLY)
        }
    };
    match id {
        LocalModelId::PaddleOcrVl16 => needs_gpu(8 * GB, 3 * GB, 6 * GB, 6 * GB),
        LocalModelId::IndexTranslate9b => needs_gpu(16 * GB, 7 * GB, 12 * GB, 16 * GB),
        LocalModelId::IndexTranslate2b => {
            if total >= 8 * GB {
                Fitness::plain(FitnessLevel::Good)
            } else if total >= 6 * GB {
                with_reason(FitnessLevel::Slow, REASON_LOW_MEMORY)
            } else {
                with_reason(FitnessLevel::Insufficient, REASON_LOW_MEMORY)
            }
        }
        LocalModelId::PpOcrV6Tiny | LocalModelId::PpOcrV6Small => Fitness::plain(FitnessLevel::Good),
    }
}

fn with_reason(level: FitnessLevel, reason: &str) -> Fitness {
    Fitness {
        level,
        reason_key: Some(reason.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use wisp_types::GpuInfo;

    use super::*;

    fn device(total_gb: u64, apple: bool, gpus: &[u32], probed: bool) -> DeviceInfo {
        named(
            total_gb,
            apple,
            &gpus.iter().map(|mb| ("NVIDIA GeForce RTX", *mb)).collect::<Vec<_>>(),
            probed,
        )
    }

    fn named(total_gb: u64, apple: bool, gpus: &[(&str, u32)], probed: bool) -> DeviceInfo {
        DeviceInfo {
            total_memory: Some(total_gb * GB),
            apple_silicon: apple,
            gpus: gpus
                .iter()
                .map(|(name, mb)| GpuInfo {
                    name: (*name).to_owned(),
                    vram_total_mb: *mb,
                    integrated: false,
                })
                .collect(),
            gpu_probed: probed,
        }
    }

    fn level(id: LocalModelId, d: &DeviceInfo, gpu: bool) -> FitnessLevel {
        assess(id, d, gpu).level
    }

    #[test]
    fn table() {
        use FitnessLevel::*;
        use LocalModelId::*;
        let mac16 = device(16, true, &[], true);
        let mac8 = device(8, true, &[], true);
        let pc_gpu = device(16, false, &[12_288], true);
        let pc_cpu = device(16, false, &[], true);
        let pc_unknown = device(16, false, &[], false);
        let small = device(4, false, &[], true);

        assert_eq!(level(PpOcrV6Tiny, &small, true), Good);
        assert_eq!(level(PaddleOcrVl16, &mac8, true), Good);
        assert_eq!(level(PaddleOcrVl16, &pc_gpu, true), Good);
        assert_eq!(level(PaddleOcrVl16, &pc_gpu, false), Slow, "关闭 GPU 加速后只有 CPU");
        assert_eq!(level(PaddleOcrVl16, &pc_cpu, true), Slow);
        assert_eq!(level(PaddleOcrVl16, &pc_unknown, true), Unknown);
        assert_eq!(level(PaddleOcrVl16, &small, true), Insufficient);

        assert_eq!(level(IndexTranslate2b, &mac8, true), Good);
        assert_eq!(level(IndexTranslate2b, &device(6, false, &[], true), true), Slow);
        assert_eq!(level(IndexTranslate2b, &small, true), Insufficient);

        assert_eq!(level(IndexTranslate9b, &mac16, true), Good);
        assert_eq!(level(IndexTranslate9b, &mac8, true), Insufficient);
        assert_eq!(level(IndexTranslate9b, &pc_gpu, true), Good);
        assert_eq!(level(IndexTranslate9b, &device(16, false, &[4_096], true), true), Slow);
        assert_eq!(level(IndexTranslate9b, &pc_unknown, true), Unknown);

        let no_mem = DeviceInfo {
            total_memory: None,
            ..pc_cpu
        };
        assert_eq!(
            assess(IndexTranslate2b, &no_mem, true).reason_key.as_deref(),
            Some(REASON_MEMORY_UNKNOWN)
        );
        assert_eq!(assess(PpOcrV6Small, &no_mem, true).level, Good);
    }

    /// 只有核显的机器按 CPU 档评估，原因写明是核显；独显与核显并存时按独显
    #[test]
    fn integrated_gpus_are_assessed_as_cpu() {
        use FitnessLevel::*;
        use LocalModelId::*;
        let igpu = named(16, false, &[("Intel(R) UHD Graphics 770", 8_159)], true);
        let fit = assess(IndexTranslate9b, &igpu, true);
        assert_eq!(fit.level, Slow);
        assert_eq!(fit.reason_key.as_deref(), Some(REASON_INTEGRATED_GPU));
        let fit = assess(PaddleOcrVl16, &igpu, true);
        assert_eq!(
            (fit.level, fit.reason_key.as_deref()),
            (Slow, Some(REASON_INTEGRATED_GPU))
        );

        let apu = named(32, false, &[("AMD Radeon 780M (RADV PHOENIX)", 16_384)], true);
        assert_eq!(
            assess(PaddleOcrVl16, &apu, true).reason_key.as_deref(),
            Some(REASON_INTEGRATED_GPU)
        );
        // 关了 GPU 加速：就是 CPU，不提核显
        assert_eq!(
            assess(PaddleOcrVl16, &apu, false).reason_key.as_deref(),
            Some(REASON_CPU_ONLY)
        );
        // 内存偏少时先说内存
        let small = named(8, false, &[("Intel(R) Iris(R) Xe Graphics", 3_900)], true);
        assert_eq!(assess(PaddleOcrVl16, &small, true).level, Slow);
        assert_eq!(assess(IndexTranslate9b, &small, true).level, Insufficient);

        let both = named(
            16,
            false,
            &[
                ("NVIDIA GeForce RTX 3060", 12_288),
                ("Intel(R) UHD Graphics 770", 8_159),
            ],
            true,
        );
        assert_eq!(assess(IndexTranslate9b, &both, true), Fitness::plain(Good));
    }
}
