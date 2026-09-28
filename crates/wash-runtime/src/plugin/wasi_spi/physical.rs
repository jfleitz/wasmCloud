//! Linux `spidev` backend for real SPI hardware.
//!
//! [`PhysicalSpi`] drives peripherals through the kernel's userspace SPI
//! interface (`/dev/spidevB.C`), e.g. APA102 LED strips wired to a Raspberry
//! Pi's MOSI/SCLK pins. Device nodes are opened lazily on the first
//! component `open` and kept open for the life of the host, so LED strips
//! keep their state between component restarts and back-to-back frames don't
//! pay open/close overhead.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use spidev::{SpiModeFlags, Spidev, SpidevOptions, SpidevTransfer};
use tokio::sync::RwLock;
use tracing::instrument;
use wasmtime::component::Resource;

use crate::{
    engine::{
        ctx::{ActiveCtx, SharedCtx, extract_active_ctx},
        workload::WorkloadItem,
    },
    plugin::{HostPlugin, WitInterfaces},
    wit::{WitInterface, WitWorld},
};

use super::{SpiDeviceConfig, WASI_SPI_ID, WASI_SPI_INTERFACE, device_configs_from_interface};

mod bindings {
    wasmtime::component::bindgen!({
        world: "spi",
        imports: { default: async | trappable | tracing },
        with: {
            "wasi:spi/spi.device": crate::plugin::wasi_spi::physical::DeviceHandle,
        },
    });
}

use bindings::wasi::spi::spi::Error as SpiError;

/// Resource representation for an opened physical device.
#[derive(Clone)]
pub struct DeviceHandle {
    dev: Arc<Mutex<Spidev>>,
    max_transfer: usize,
}

struct CachedDevice {
    dev: Arc<Mutex<Spidev>>,
    config: SpiDeviceConfig,
}

/// Physical SPI plugin backed by Linux `/dev/spidev*` device nodes.
///
/// Components open logical device names; the name-to-node mapping comes from
/// the `devices` map given at construction, optionally extended or overridden
/// per workload through `wasi:spi` interface config (see the
/// [module docs](super)).
///
/// # Examples
///
/// ```no_run
/// use std::collections::HashMap;
/// use wash_runtime::plugin::wasi_spi::{PhysicalSpi, SpiDeviceConfig};
///
/// let _plugin = PhysicalSpi::builder()
///     .devices(HashMap::from([(
///         "playfield-leds".to_string(),
///         SpiDeviceConfig::new("/dev/spidev0.0"),
///     )]))
///     .build();
/// ```
#[derive(Clone, bon::Builder)]
pub struct PhysicalSpi {
    /// Logical device name to hardware configuration.
    #[builder(default)]
    devices: HashMap<String, SpiDeviceConfig>,
    /// Maximum bytes per single write/read/transfer. Defaults to 4096, the
    /// kernel's default `spidev.bufsiz`; raise both together for longer
    /// APA102 strips.
    #[builder(default = 4096)]
    max_transfer: usize,
    /// Per-workload device definitions from interface config, layered over
    /// `devices`.
    #[builder(skip)]
    overrides: Arc<RwLock<HashMap<String, HashMap<String, SpiDeviceConfig>>>>,
    /// Opened device nodes, keyed by path and shared across workloads.
    #[builder(skip)]
    opened: Arc<RwLock<HashMap<PathBuf, CachedDevice>>>,
}

impl PhysicalSpi {
    /// Resolves the configuration for `name` as seen by `workload_id`.
    async fn resolve_config(&self, workload_id: &str, name: &str) -> Option<SpiDeviceConfig> {
        let overrides = self.overrides.read().await;
        overrides
            .get(workload_id)
            .and_then(|devices| devices.get(name))
            .or_else(|| self.devices.get(name))
            .cloned()
    }

    /// Returns the shared handle for `config.path`, opening and configuring
    /// the device node on first use.
    async fn open_device(&self, config: &SpiDeviceConfig) -> Result<Arc<Mutex<Spidev>>, SpiError> {
        let mut opened = self.opened.write().await;
        if let Some(cached) = opened.get(&config.path) {
            if cached.config != *config {
                tracing::warn!(
                    path = %config.path.display(),
                    "SPI device already opened with different settings; reusing existing configuration"
                );
            }
            return Ok(cached.dev.clone());
        }

        let config_for_open = config.clone();
        let dev = tokio::task::spawn_blocking(move || open_and_configure(&config_for_open))
            .await
            .map_err(|e| SpiError::Other(format!("SPI open task failed: {e}")))?
            .map_err(|e| SpiError::Io(e.to_string()))?;
        let dev = Arc::new(Mutex::new(dev));
        opened.insert(
            config.path.clone(),
            CachedDevice {
                dev: dev.clone(),
                config: config.clone(),
            },
        );
        Ok(dev)
    }
}

fn open_and_configure(config: &SpiDeviceConfig) -> std::io::Result<Spidev> {
    let mut flags = match config.mode {
        0 => SpiModeFlags::SPI_MODE_0,
        1 => SpiModeFlags::SPI_MODE_1,
        2 => SpiModeFlags::SPI_MODE_2,
        3 => SpiModeFlags::SPI_MODE_3,
        other => {
            return Err(std::io::Error::other(format!(
                "SPI mode must be 0-3, got {other}"
            )));
        }
    };
    if config.lsb_first {
        flags |= SpiModeFlags::SPI_LSB_FIRST;
    }

    let mut dev = Spidev::open(&config.path)?;
    let options = SpidevOptions::new()
        .bits_per_word(config.bits_per_word)
        .max_speed_hz(config.speed_hz)
        .mode(flags)
        .build();
    dev.configure(&options)?;
    Ok(dev)
}

/// Runs one blocking spidev transfer on the blocking thread pool, keeping
/// the wasmtime executor free while the bus clocks bytes out.
async fn run_transfer<T, F>(dev: Arc<Mutex<Spidev>>, op: F) -> wasmtime::Result<Result<T, SpiError>>
where
    T: Send + 'static,
    F: FnOnce(&Spidev) -> std::io::Result<T> + Send + 'static,
{
    let result = tokio::task::spawn_blocking(move || {
        let guard = dev
            .lock()
            .map_err(|_| std::io::Error::other("SPI device lock poisoned"))?;
        op(&guard)
    })
    .await?;
    Ok(result.map_err(|e| SpiError::Io(e.to_string())))
}

impl<'a> bindings::wasi::spi::spi::Host for ActiveCtx<'a> {
    #[instrument(name = "wasi.spi.open", skip(self))]
    async fn open(
        &mut self,
        name: String,
    ) -> wasmtime::Result<Result<Resource<DeviceHandle>, SpiError>> {
        let workload_id = self.workload_id.to_string();
        let plugin = self.try_get_plugin::<PhysicalSpi>(WASI_SPI_ID)?;

        let Some(config) = plugin.resolve_config(&workload_id, &name).await else {
            return Ok(Err(SpiError::NoSuchDevice));
        };
        let dev = match plugin.open_device(&config).await {
            Ok(dev) => dev,
            Err(e) => return Ok(Err(e)),
        };

        let resource = self.table.push(DeviceHandle {
            dev,
            max_transfer: plugin.max_transfer,
        })?;
        Ok(Ok(resource))
    }
}

impl<'a> bindings::wasi::spi::spi::HostDevice for ActiveCtx<'a> {
    #[instrument(name = "wasi.spi.write", skip(self, device, data), fields(len = data.len()))]
    async fn write(
        &mut self,
        device: Resource<DeviceHandle>,
        data: Vec<u8>,
    ) -> wasmtime::Result<Result<(), SpiError>> {
        let handle = self.table.get(&device)?.clone();
        if data.len() > handle.max_transfer {
            return Ok(Err(oversized(data.len(), handle.max_transfer)));
        }
        run_transfer(handle.dev, move |dev| {
            let mut transfer = SpidevTransfer::write(&data);
            dev.transfer(&mut transfer)
        })
        .await
    }

    #[instrument(name = "wasi.spi.read", skip(self, device))]
    async fn read(
        &mut self,
        device: Resource<DeviceHandle>,
        len: u32,
    ) -> wasmtime::Result<Result<Vec<u8>, SpiError>> {
        let handle = self.table.get(&device)?.clone();
        let len = len as usize;
        if len > handle.max_transfer {
            return Ok(Err(oversized(len, handle.max_transfer)));
        }
        run_transfer(handle.dev, move |dev| {
            let mut rx = vec![0; len];
            {
                let mut transfer = SpidevTransfer::read(&mut rx);
                dev.transfer(&mut transfer)?;
            }
            Ok(rx)
        })
        .await
    }

    #[instrument(name = "wasi.spi.transfer", skip(self, device, data), fields(len = data.len()))]
    async fn transfer(
        &mut self,
        device: Resource<DeviceHandle>,
        data: Vec<u8>,
    ) -> wasmtime::Result<Result<Vec<u8>, SpiError>> {
        let handle = self.table.get(&device)?.clone();
        if data.len() > handle.max_transfer {
            return Ok(Err(oversized(data.len(), handle.max_transfer)));
        }
        run_transfer(handle.dev, move |dev| {
            let mut rx = vec![0; data.len()];
            {
                let mut transfer = SpidevTransfer::read_write(&data, &mut rx);
                dev.transfer(&mut transfer)?;
            }
            Ok(rx)
        })
        .await
    }

    async fn drop(&mut self, rep: Resource<DeviceHandle>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

fn oversized(len: usize, max: usize) -> SpiError {
    SpiError::InvalidArgument(format!(
        "transfer of {len} bytes exceeds the host limit of {max} bytes"
    ))
}

#[async_trait::async_trait]
impl HostPlugin for PhysicalSpi {
    fn id(&self) -> &'static str {
        WASI_SPI_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: HashSet::from([WitInterface::from(WASI_SPI_INTERFACE)]),
            ..Default::default()
        }
    }

    async fn on_workload_item_bind<'a>(
        &self,
        component_handle: &mut WorkloadItem<'a>,
        interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        let Some(interface) = interfaces.get("wasi", "spi", &["spi"]) else {
            tracing::warn!(
                "PhysicalSpi plugin requested for non-wasi:spi interface(s): {:?}",
                interfaces
            );
            return Ok(());
        };

        bindings::wasi::spi::spi::add_to_linker::<_, SharedCtx>(
            component_handle.linker(),
            extract_active_ctx,
        )?;

        let devices = device_configs_from_interface(&interface.config)?;
        if !devices.is_empty() {
            self.overrides
                .write()
                .await
                .insert(component_handle.workload_id().to_string(), devices);
        }

        tracing::debug!(
            workload_id = component_handle.workload_id(),
            "PhysicalSpi plugin bound to workload"
        );
        Ok(())
    }

    async fn on_workload_unbind(
        &self,
        workload_id: &str,
        _interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        self.overrides.write().await.remove(workload_id);
        tracing::debug!("PhysicalSpi plugin unbound from workload '{workload_id}'");
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        // Drop cached device nodes so the fds close on host shutdown.
        self.opened.write().await.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_prefers_workload_overrides() {
        let plugin = PhysicalSpi::builder()
            .devices(HashMap::from([(
                "leds".to_string(),
                SpiDeviceConfig::new("/dev/spidev0.0"),
            )]))
            .build();
        plugin.overrides.write().await.insert(
            "w1".to_string(),
            HashMap::from([("leds".to_string(), SpiDeviceConfig::new("/dev/spidev0.1"))]),
        );

        let overridden = plugin.resolve_config("w1", "leds").await;
        assert_eq!(
            overridden.map(|c| c.path),
            Some(PathBuf::from("/dev/spidev0.1"))
        );

        let base = plugin.resolve_config("w2", "leds").await;
        assert_eq!(base.map(|c| c.path), Some(PathBuf::from("/dev/spidev0.0")));

        assert!(plugin.resolve_config("w1", "unknown").await.is_none());
    }

    #[test]
    fn rejects_invalid_mode_at_open() {
        let mut config = SpiDeviceConfig::new("/dev/spidev0.0");
        config.mode = 4;
        assert!(open_and_configure(&config).is_err());
    }

    #[test]
    fn oversized_error_mentions_limit() {
        match oversized(5000, 4096) {
            SpiError::InvalidArgument(msg) => {
                assert!(msg.contains("5000") && msg.contains("4096"));
            }
            other => panic!("expected invalid-argument, got {other:?}"),
        }
    }
}
