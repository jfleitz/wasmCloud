//! Machine-config file access for the wasm-pinball-framework.
//!
//! Implements `wpf:config-loader/includes` — the filesystem callback the
//! framework's `config-loader` component (and its boot driver) import to read
//! MPF-style machine config files. Wasm components have no ambient
//! filesystem; this plugin serves reads out of one operator-configured
//! **machine root** per workload, so a workload can see exactly its machine
//! folder and nothing else.
//!
//! # Path scoping
//!
//! `fetch(base-label, relative-path)` resolves `relative-path` against the
//! directory of `base-label` — the same scoping MPF Python gives `!include`
//! tags (a file's includes are relative to the file). The empty base label
//! addresses the machine root itself, so a boot driver starts with
//! `fetch("", "config/config.yaml")` and follow-up reads use the previous
//! label: `fetch("config/config.yaml", "switches.yaml")` →
//! `<root>/config/switches.yaml`. Every resolved path is locked under the
//! root: absolute paths and `..` traversal are rejected outright.
//!
//! # Configuration
//!
//! Workloads declare the root as `wpf:config-loader` interface config:
//!
//! ```text
//! root = /home/pinball/machines/cosmic-carnival   # required, must exist
//! ```
//!
//! The root is tracked per workload, so two machines on one host stay
//! isolated.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tracing::{debug, instrument, warn};

use crate::engine::ctx::{ActiveCtx, SharedCtx, extract_active_ctx};
use crate::engine::workload::WorkloadItem;
use crate::plugin::{HostPlugin, WitInterfaces, lock_root};
use crate::wit::{WitInterface, WitWorld};

const WPF_CONFIG_INCLUDES_ID: &str = "wpf-config-includes";
const WPF_CONFIG_INCLUDES_INTERFACE: &str = "wpf:config-loader/includes";

mod bindings {
    wasmtime::component::bindgen!({
        world: "wpf-config-includes",
        imports: { default: async | trappable | tracing },
    });
}

use bindings::wpf::config_loader::includes::{Host, IncludeError};

/// Root-locked machine-config file access (`wpf:config-loader/includes`).
///
/// One plugin instance per host; each bound workload gets the machine root
/// its manifest configured.
#[derive(Default)]
pub struct ConfigIncludes {
    roots: std::sync::Mutex<HashMap<String, PathBuf>>,
}

impl ConfigIncludes {
    fn root_for(&self, workload_id: &str) -> Option<PathBuf> {
        let roots = lock_unpoisoned(&self.roots);
        roots.get(workload_id).cloned()
    }
}

/// Locks a mutex, recovering the guard if a previous holder panicked. The
/// only state behind this lock is the root map, which stays internally
/// consistent across a panic, so continuing is safe.
fn lock_unpoisoned<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Resolves `relative` against the directory of `base_label`, locked under
/// `root`. Returns the absolute path, or a message describing the rejection.
fn resolve_within_root(root: &Path, base_label: &str, relative: &str) -> Result<PathBuf, String> {
    let base_dir = Path::new(base_label).parent().unwrap_or(Path::new(""));
    let candidate = base_dir.join(relative);
    let candidate = candidate.to_str().ok_or("non-UTF-8 path")?;
    lock_root(root, candidate).map_err(str::to_string)
}

impl Host for ActiveCtx<'_> {
    #[instrument(name = "wpf.config_loader.includes.fetch", skip_all, fields(base = %base_label, path = %relative_path))]
    async fn fetch(
        &mut self,
        base_label: String,
        relative_path: String,
    ) -> wasmtime::Result<Result<Vec<u8>, IncludeError>> {
        let plugin = self.try_get_plugin::<ConfigIncludes>(WPF_CONFIG_INCLUDES_ID)?;
        let Some(root) = plugin.root_for(&self.workload_id) else {
            return Ok(Err(IncludeError::Io(
                "no machine root configured for this workload \
                 (set `root` in the wpf:config-loader interface config)"
                    .to_string(),
            )));
        };
        let path = match resolve_within_root(&root, &base_label, &relative_path) {
            Ok(path) => path,
            Err(reason) => {
                warn!(%base_label, %relative_path, reason, "rejected includes fetch");
                return Ok(Err(IncludeError::Io(format!(
                    "invalid path '{relative_path}' (relative to '{base_label}'): {reason}"
                ))));
            }
        };
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Ok(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Err(IncludeError::NotFound(
                format!("{relative_path} (relative to {base_label})"),
            ))),
            Err(e) => Ok(Err(IncludeError::Io(format!(
                "reading {}: {e}",
                path.display()
            )))),
        }
    }
}

#[async_trait::async_trait]
impl HostPlugin for ConfigIncludes {
    fn id(&self) -> &'static str {
        WPF_CONFIG_INCLUDES_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: HashSet::from([WitInterface::from(WPF_CONFIG_INCLUDES_INTERFACE)]),
            ..Default::default()
        }
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        let Some(interface) = interfaces.get("wpf", "config-loader", &["includes"]) else {
            warn!(
                "ConfigIncludes plugin requested for non-wpf:config-loader/includes \
                 interface(s): {:?}",
                interfaces
            );
            return Ok(());
        };

        // Validate at bind so a bad manifest fails the deploy, not the first
        // config read.
        let root = interface.config.get("root").ok_or_else(|| {
            anyhow::anyhow!(
                "wpf:config-loader/includes requires a 'root' config key \
                 (absolute path of the machine folder)"
            )
        })?;
        let root = PathBuf::from(root);
        anyhow::ensure!(
            root.is_absolute(),
            "wpf:config-loader/includes 'root' must be an absolute path, got {}",
            root.display()
        );
        anyhow::ensure!(
            root.is_dir(),
            "wpf:config-loader/includes 'root' is not a directory on this host: {}",
            root.display()
        );

        {
            let mut roots = lock_unpoisoned(&self.roots);
            roots.insert(item.workload_id().to_string(), root.clone());
        }

        bindings::wpf::config_loader::includes::add_to_linker::<_, SharedCtx>(
            item.linker(),
            extract_active_ctx,
        )?;

        debug!(
            workload_id = item.workload_id(),
            root = %root.display(),
            "ConfigIncludes plugin bound to workload"
        );
        Ok(())
    }

    async fn on_workload_unbind(
        &self,
        workload_id: &str,
        _interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        let mut roots = lock_unpoisoned(&self.roots);
        roots.remove(workload_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Path-resolution seams only — the filesystem read is a straight
    //! `tokio::fs::read` once the path survives `resolve_within_root`.
    #![allow(clippy::unwrap_used)]
    use super::*;

    const ROOT: &str = "/machines/cosmic-carnival";

    #[test]
    fn empty_base_label_addresses_the_root() {
        let path = resolve_within_root(Path::new(ROOT), "", "config/config.yaml").unwrap();
        assert_eq!(path, Path::new(ROOT).join("config/config.yaml"));
    }

    #[test]
    fn relative_paths_scope_to_the_base_labels_directory() {
        let path =
            resolve_within_root(Path::new(ROOT), "config/config.yaml", "switches.yaml").unwrap();
        assert_eq!(path, Path::new(ROOT).join("config/switches.yaml"));
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        for (base, rel) in [
            ("config/config.yaml", "../secrets.yaml"),
            ("", "/etc/passwd"),
            ("../outside.yaml", "x.yaml"),
        ] {
            assert!(
                resolve_within_root(Path::new(ROOT), base, rel).is_err(),
                "{base} + {rel} must be rejected"
            );
        }
    }
}
