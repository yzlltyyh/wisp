//! 由库存、下载任务、槽位状态、设备与设置组装设置页的快照（纯函数）。

use std::collections::BTreeMap;

use wisp_local::catalog::{self, Target};
use wisp_local::fitness;
use wisp_local::store::{InstallState, Inventory};
use wisp_types::{
    DeviceInfo, LoadState, LocalModelId, LocalModelInfo, LocalModelState, LocalModelsSnapshot, LocalRuntimeId,
    LocalRuntimeInfo, OfflineSettings,
};

/// 组装快照的输入
pub(super) struct Inputs<'a> {
    pub inventory: &'a Inventory,
    pub device: DeviceInfo,
    pub offline: &'a OfflineSettings,
    /// 当前发布目标（不在发布范围时运行时的下载量为 0）
    pub target: Option<Target>,
    /// 下载队列里的模型的状态（排队、下载中、校验中、失败），优先于库存
    pub active: &'a BTreeMap<LocalModelId, LocalModelState>,
    /// 槽位的加载状态
    pub loads: &'a BTreeMap<LocalModelId, LoadState>,
    pub disk_free: Option<u64>,
}

pub(super) fn build(i: &Inputs<'_>) -> LocalModelsSnapshot {
    let inv = i.inventory;
    // 推理组件实际要下载的字节数（区间提取时只是其中一个文件），与下载器的进度总量同一口径
    let runtime_bytes = |r: LocalRuntimeId| {
        i.target
            .and_then(|t| catalog::runtime_asset(r, t))
            .map_or(0, |a| a.download_bytes())
    };
    let models = LocalModelId::ALL
        .into_iter()
        .map(|id| {
            let spec = catalog::model(id);
            let installed = inv.models.get(&id);
            let pending_runtime_bytes: u64 = spec
                .runtimes
                .iter()
                .filter(|r| !inv.runtime_installed(**r))
                .map(|r| runtime_bytes(*r))
                .sum();
            let state = i.active.get(&id).copied().unwrap_or_else(|| match installed {
                Some(InstallState::Installed { .. }) => LocalModelState::Installed,
                Some(InstallState::Broken) => LocalModelState::Broken,
                Some(InstallState::NotInstalled) | None => match inv.partial.get(&id) {
                    // 有部分文件、又不在下载队列里：上次暂停（或退出时中断）的下载。部分文件含一并下载的推理组件的
                    // `.part`，总量因此也算上还没装的推理组件（与继续下载时的进度条一致，不会超过 100%、也不倒退）
                    Some(done) if *done > 0 => {
                        let total = catalog::model_download_bytes(id) + pending_runtime_bytes;
                        LocalModelState::Paused {
                            done: (*done).min(total),
                            total,
                        }
                    }
                    _ => LocalModelState::NotInstalled,
                },
            });
            LocalModelInfo {
                id,
                kind: id.kind(),
                download_bytes: catalog::model_download_bytes(id),
                pending_runtime_bytes,
                state,
                load: i.loads.get(&id).copied().unwrap_or_default(),
                memory_estimate_mb: spec.memory_estimate_mb,
                fitness: fitness::assess(id, &i.device, i.offline.use_gpu),
                update_available: matches!(installed, Some(InstallState::Installed { stale: true })),
            }
        })
        .collect();
    let runtimes = LocalRuntimeId::ALL
        .into_iter()
        .map(|id| {
            let installed = inv.runtime_installed(id);
            LocalRuntimeInfo {
                id,
                version: catalog::runtime(id).version.to_owned(),
                installed,
                size: if installed {
                    inv.runtimes.get(&id).map_or(0, |r| r.size)
                } else {
                    runtime_bytes(id)
                },
                in_use: LocalModelId::ALL
                    .into_iter()
                    .any(|m| inv.is_installed(m) && catalog::model(m).runtimes.contains(&id)),
            }
        })
        .collect();
    LocalModelsSnapshot {
        models,
        runtimes,
        device: i.device.clone(),
        disk_used: inv.disk_used,
        disk_free: i.disk_free,
    }
}

#[cfg(test)]
mod tests {
    use wisp_local::store::RuntimeInstall;

    use super::*;

    fn device() -> DeviceInfo {
        DeviceInfo {
            total_memory: Some(16 * 1024 * 1024 * 1024),
            apple_silicon: true,
            gpus: Vec::new(),
            gpu_probed: true,
        }
    }

    #[test]
    fn empty_inventory_lists_everything_as_not_installed() {
        let inv = Inventory::default();
        let snap = build(&Inputs {
            inventory: &inv,
            device: device(),
            offline: &OfflineSettings::default(),
            target: Target::current(),
            active: &BTreeMap::new(),
            loads: &BTreeMap::new(),
            disk_free: Some(1),
        });
        assert_eq!(snap.models.len(), LocalModelId::ALL.len());
        assert!(snap.models.iter().all(|m| m.state == LocalModelState::NotInstalled));
        assert!(snap.models.iter().all(|m| m.load == LoadState::Unloaded));
        assert_eq!(snap.runtimes.len(), LocalRuntimeId::ALL.len());
        assert!(snap.runtimes.iter().all(|r| !r.installed && !r.in_use));
    }

    #[test]
    fn installed_models_mark_their_runtimes_in_use_and_partials_are_paused() {
        let mut inv = Inventory::default();
        inv.models
            .insert(LocalModelId::PpOcrV6Small, InstallState::Installed { stale: true });
        inv.runtimes.insert(
            LocalRuntimeId::OnnxRuntime,
            RuntimeInstall {
                installed: true,
                size: 21_000_000,
            },
        );
        inv.partial.insert(LocalModelId::IndexTranslate2b, 1_000);
        let mut active = BTreeMap::new();
        active.insert(LocalModelId::IndexTranslate9b, LocalModelState::Queued);
        let mut loads = BTreeMap::new();
        loads.insert(LocalModelId::PpOcrV6Small, LoadState::Loaded);
        let snap = build(&Inputs {
            inventory: &inv,
            device: device(),
            offline: &OfflineSettings::default(),
            target: Target::current(),
            active: &active,
            loads: &loads,
            disk_free: None,
        });
        let m = |id: LocalModelId| snap.models.iter().find(|m| m.id == id).expect("model");
        assert_eq!(m(LocalModelId::PpOcrV6Small).state, LocalModelState::Installed);
        assert!(m(LocalModelId::PpOcrV6Small).update_available);
        assert_eq!(m(LocalModelId::PpOcrV6Small).load, LoadState::Loaded);
        assert_eq!(m(LocalModelId::PpOcrV6Small).pending_runtime_bytes, 0);
        assert!(matches!(
            m(LocalModelId::IndexTranslate2b).state,
            LocalModelState::Paused { done: 1_000, .. }
        ));
        assert_eq!(m(LocalModelId::IndexTranslate9b).state, LocalModelState::Queued);
        let ort = snap
            .runtimes
            .iter()
            .find(|r| r.id == LocalRuntimeId::OnnxRuntime)
            .expect("ort");
        assert!(ort.installed && ort.in_use);
        assert_eq!(ort.size, 21_000_000);
    }

    #[test]
    fn a_paused_download_counts_the_runtimes_it_still_needs() {
        let Some(target) = Target::current() else { return };
        // 推理组件的 `.part` 也在模型的部分文件里：暂停时的总量要算上还没装的推理组件
        let model = LocalModelId::PpOcrV6Tiny;
        let ort = catalog::runtime_asset(LocalRuntimeId::OnnxRuntime, target)
            .expect("ort asset")
            .download_bytes();
        let model_bytes = catalog::model_download_bytes(model);
        let mut inv = Inventory::default();
        inv.partial.insert(model, ort / 2);
        let snap = build(&Inputs {
            inventory: &inv,
            device: device(),
            offline: &OfflineSettings::default(),
            target: Some(target),
            active: &BTreeMap::new(),
            loads: &BTreeMap::new(),
            disk_free: None,
        });
        let m = snap.models.iter().find(|m| m.id == model).expect("model");
        assert_eq!(m.pending_runtime_bytes, ort);
        assert_eq!(
            m.state,
            LocalModelState::Paused {
                done: ort / 2,
                total: model_bytes + ort
            }
        );
        let r = snap
            .runtimes
            .iter()
            .find(|r| r.id == LocalRuntimeId::OnnxRuntime)
            .expect("ort");
        assert_eq!(r.size, ort, "没装时显示实际要下载的大小");
        // 推理组件装好之后只剩模型文件
        inv.runtimes.insert(
            LocalRuntimeId::OnnxRuntime,
            RuntimeInstall {
                installed: true,
                size: 1,
            },
        );
        inv.partial.insert(model, model_bytes * 2);
        let snap = build(&Inputs {
            inventory: &inv,
            device: device(),
            offline: &OfflineSettings::default(),
            target: Some(target),
            active: &BTreeMap::new(),
            loads: &BTreeMap::new(),
            disk_free: None,
        });
        let m = snap.models.iter().find(|m| m.id == model).expect("model");
        assert_eq!(
            m.state,
            LocalModelState::Paused {
                done: model_bytes,
                total: model_bytes
            },
            "不超过 100%"
        );
    }
}
