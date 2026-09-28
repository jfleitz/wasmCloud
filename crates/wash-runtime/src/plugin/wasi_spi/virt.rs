//! In-memory SPI backend for tests and virtual hardware.
//!
//! [`VirtualSpi`] accepts any device name and records every frame written to
//! it, so a virtual-hardware harness (e.g. a simulated pinball playfield) can
//! inspect the exact bytes a component would have clocked out to real LEDs.
//! Reads and the read half of transfers return zeros.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

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

use super::{WASI_SPI_ID, WASI_SPI_INTERFACE};

mod bindings {
    wasmtime::component::bindgen!({
        world: "spi",
        imports: { default: async | trappable | tracing },
        with: {
            "wasi:spi/spi.device": crate::plugin::wasi_spi::virt::DeviceName,
        },
    });
}

use bindings::wasi::spi::spi::Error as SpiError;

/// Resource representation for an opened virtual device.
pub type DeviceName = String;

/// Frames written per device, oldest first.
type DeviceFrames = HashMap<String, VecDeque<Vec<u8>>>;

/// Virtual SPI plugin that captures written frames in memory.
///
/// Any device name can be opened. Construct via [`VirtualSpi::builder`] (or
/// [`Default::default`]) and inspect captured traffic with
/// [`VirtualSpi::written_frames`] / [`VirtualSpi::take_frames`].
///
/// # Examples
///
/// ```
/// use wash_runtime::plugin::wasi_spi::VirtualSpi;
///
/// let _plugin = VirtualSpi::builder().max_frames(64).build();
/// ```
#[derive(Clone, bon::Builder)]
pub struct VirtualSpi {
    /// Maximum number of frames retained per device; older frames are
    /// dropped first. Keeps a component animating LEDs at hundreds of
    /// frames per second from growing memory without bound.
    #[builder(default = 256)]
    max_frames: usize,
    /// Captured frames, keyed by workload ID then device name.
    #[builder(skip)]
    frames: Arc<RwLock<HashMap<String, DeviceFrames>>>,
}

impl Default for VirtualSpi {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl VirtualSpi {
    /// Returns copies of the frames written to `device` by `workload_id`,
    /// oldest first.
    pub async fn written_frames(&self, workload_id: &str, device: &str) -> Vec<Vec<u8>> {
        let frames = self.frames.read().await;
        frames
            .get(workload_id)
            .and_then(|devices| devices.get(device))
            .map(|frames| frames.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Removes and returns the frames written to `device` by `workload_id`,
    /// oldest first.
    pub async fn take_frames(&self, workload_id: &str, device: &str) -> Vec<Vec<u8>> {
        let mut frames = self.frames.write().await;
        frames
            .get_mut(workload_id)
            .and_then(|devices| devices.get_mut(device))
            .map(|frames| std::mem::take(frames).into_iter().collect())
            .unwrap_or_default()
    }

    async fn record_frame(&self, workload_id: &str, device: &str, data: Vec<u8>) {
        let mut frames = self.frames.write().await;
        let device_frames = frames
            .entry(workload_id.to_string())
            .or_default()
            .entry(device.to_string())
            .or_default();
        device_frames.push_back(data);
        while device_frames.len() > self.max_frames {
            device_frames.pop_front();
        }
    }
}

impl<'a> bindings::wasi::spi::spi::Host for ActiveCtx<'a> {
    #[instrument(name = "wasi.spi.open", skip(self))]
    async fn open(
        &mut self,
        name: String,
    ) -> wasmtime::Result<Result<Resource<DeviceName>, SpiError>> {
        let plugin = self.try_get_plugin::<VirtualSpi>(WASI_SPI_ID)?;

        // Register the device eagerly so written_frames() distinguishes
        // "opened but never written" from "never opened".
        let mut frames = plugin.frames.write().await;
        frames
            .entry(self.workload_id.to_string())
            .or_default()
            .entry(name.clone())
            .or_default();
        drop(frames);

        let resource = self.table.push(name)?;
        Ok(Ok(resource))
    }
}

impl<'a> bindings::wasi::spi::spi::HostDevice for ActiveCtx<'a> {
    #[instrument(name = "wasi.spi.write", skip(self, device, data), fields(len = data.len()))]
    async fn write(
        &mut self,
        device: Resource<DeviceName>,
        data: Vec<u8>,
    ) -> wasmtime::Result<Result<(), SpiError>> {
        let name = self.table.get(&device)?.clone();
        let workload_id = self.workload_id.to_string();
        let plugin = self.try_get_plugin::<VirtualSpi>(WASI_SPI_ID)?;
        plugin.record_frame(&workload_id, &name, data).await;
        Ok(Ok(()))
    }

    #[instrument(name = "wasi.spi.read", skip(self, device))]
    async fn read(
        &mut self,
        device: Resource<DeviceName>,
        len: u32,
    ) -> wasmtime::Result<Result<Vec<u8>, SpiError>> {
        let _ = self.table.get(&device)?;
        Ok(Ok(vec![0; len as usize]))
    }

    #[instrument(name = "wasi.spi.transfer", skip(self, device, data), fields(len = data.len()))]
    async fn transfer(
        &mut self,
        device: Resource<DeviceName>,
        data: Vec<u8>,
    ) -> wasmtime::Result<Result<Vec<u8>, SpiError>> {
        let name = self.table.get(&device)?.clone();
        let workload_id = self.workload_id.to_string();
        let len = data.len();
        let plugin = self.try_get_plugin::<VirtualSpi>(WASI_SPI_ID)?;
        plugin.record_frame(&workload_id, &name, data).await;
        Ok(Ok(vec![0; len]))
    }

    async fn drop(&mut self, rep: Resource<DeviceName>) -> wasmtime::Result<()> {
        self.table.delete(rep)?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl HostPlugin for VirtualSpi {
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
        if !interfaces.contains("wasi", "spi", &["spi"]) {
            tracing::warn!(
                "VirtualSpi plugin requested for non-wasi:spi interface(s): {:?}",
                interfaces
            );
            return Ok(());
        }

        bindings::wasi::spi::spi::add_to_linker::<_, SharedCtx>(
            component_handle.linker(),
            extract_active_ctx,
        )?;

        tracing::debug!(
            workload_id = component_handle.workload_id(),
            "VirtualSpi plugin bound to workload"
        );
        Ok(())
    }

    async fn on_workload_unbind(
        &self,
        workload_id: &str,
        _interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        self.frames.write().await.remove(workload_id);
        tracing::debug!("VirtualSpi plugin unbound from workload '{workload_id}'");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn records_frames_in_order() {
        let spi = VirtualSpi::default();
        spi.record_frame("w1", "leds", vec![0x00, 0x00, 0x00, 0x00])
            .await;
        spi.record_frame("w1", "leds", vec![0xE0 | 31, 0xFF, 0x00, 0x00])
            .await;

        let frames = spi.written_frames("w1", "leds").await;
        assert_eq!(
            frames,
            vec![
                vec![0x00, 0x00, 0x00, 0x00],
                vec![0xE0 | 31, 0xFF, 0x00, 0x00]
            ]
        );
    }

    #[tokio::test]
    async fn isolates_workloads_and_devices() {
        let spi = VirtualSpi::default();
        spi.record_frame("w1", "leds", vec![1]).await;
        spi.record_frame("w2", "leds", vec![2]).await;
        spi.record_frame("w1", "matrix", vec![3]).await;

        assert_eq!(spi.written_frames("w1", "leds").await, vec![vec![1]]);
        assert_eq!(spi.written_frames("w2", "leds").await, vec![vec![2]]);
        assert_eq!(spi.written_frames("w1", "matrix").await, vec![vec![3]]);
        assert!(spi.written_frames("w3", "leds").await.is_empty());
    }

    #[tokio::test]
    async fn caps_retained_frames() {
        let spi = VirtualSpi::builder().max_frames(2).build();
        spi.record_frame("w1", "leds", vec![1]).await;
        spi.record_frame("w1", "leds", vec![2]).await;
        spi.record_frame("w1", "leds", vec![3]).await;

        assert_eq!(
            spi.written_frames("w1", "leds").await,
            vec![vec![2], vec![3]]
        );
    }

    #[tokio::test]
    async fn take_frames_drains() {
        let spi = VirtualSpi::default();
        spi.record_frame("w1", "leds", vec![1]).await;

        assert_eq!(spi.take_frames("w1", "leds").await, vec![vec![1]]);
        assert!(spi.written_frames("w1", "leds").await.is_empty());
    }

    #[tokio::test]
    async fn unbind_clears_workload_frames() {
        let spi = VirtualSpi::default();
        spi.record_frame("w1", "leds", vec![1]).await;
        spi.frames.write().await.remove("w1");
        assert!(spi.written_frames("w1", "leds").await.is_empty());
    }
}
