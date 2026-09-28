//! `wpf:hardware/controller` host plugin backed by a Multimorphic P3-ROC.
//!
//! Binds the wasip3 controller interface (async commands, event stream) and
//! translates it onto the P-ROC wire protocol via the [`device`](super::device)
//! thread. Semantics worth knowing:
//!
//! - **Command acks**: the P-ROC protocol has no per-command acknowledgement;
//!   a command resolves once its words are accepted by the USB FIFO. Errors
//!   after that point surface as `link-down` on the event stream.
//! - **Coil numbering**: `coil-id` is used as the P3-ROC driver number
//!   verbatim (PD-16 board `B` bank `K` output `N` = `B*16 + K*8 + N`). The
//!   machine config in `device-manager` owns the mapping from playfield names
//!   to these numbers.
//! - **`hold-coil` PWM**: implemented with the driver "patter" engine. The
//!   duty cycle is `pwm/255` over a [`P3Roc::pwm_period_ms`] window; 255 is a
//!   solid hold, 0 releases.
//! - **`configure-autofire`**: becomes a firmware switch rule on the
//!   closed-nondebounced transition. `cooldown-ms > 0` maps to the rule's
//!   reload throttle (the firmware's reload window is fixed, not per-rule
//!   programmable). The host keeps receiving the debounced notify events, so
//!   `device-manager` still sees the switch.
//! - **`send-raw`**: writes the payload as raw big-endian 32-bit words to the
//!   FPGA bus and returns an empty response; it is a diagnostics hatch, not a
//!   read path.

use std::collections::{HashMap, HashSet};

use crate::engine::ctx::{ActiveCtx, SharedCtx, extract_active_ctx};
use crate::engine::workload::WorkloadItem;
use crate::plugin::{HostPlugin, WitInterfaces};
use crate::wit::{WitInterface, WitWorld};

use super::device::{DeviceConfig, DeviceShared, HwCommand, HwError, HwEvent};
use super::proto::{
    self, DriverState, P3_ROC_SWITCH_STATE_BASE_ADDR, SwitchRule, SwitchTransition,
};
use super::{WPF_HARDWARE_ID, WPF_HARDWARE_INTERFACE};

mod bindings {
    wasmtime::component::bindgen!({
        world: "wpf-hardware",
        imports: { default: async | trappable | tracing },
    });
}

use bindings::wpf::hardware::controller::{FastError, FastEvent, SwitchEvent};

impl From<HwError> for FastError {
    fn from(e: HwError) -> Self {
        match e {
            HwError::NotConnected => FastError::NotConnected,
            HwError::Timeout => FastError::Timeout,
            HwError::Protocol(s) => FastError::Protocol(s),
            HwError::Io(s) => FastError::Io(s),
        }
    }
}

impl From<HwEvent> for FastEvent {
    fn from(e: HwEvent) -> Self {
        match e {
            HwEvent::Switch {
                switch_id,
                closed,
                timestamp_us,
            } => FastEvent::Switch(SwitchEvent {
                switch_id,
                closed,
                timestamp_us,
            }),
            HwEvent::LinkUp => FastEvent::LinkUp,
            HwEvent::LinkDown => FastEvent::LinkDown,
        }
    }
}

/// A P3-ROC coil/switch controller plugin (`wpf:hardware/controller@0.2.0`).
///
/// One plugin instance drives one physical P3-ROC. Device settings come from
/// the builder and can be overridden by `wpf:hardware` interface config on
/// the workload (flat keys: `serial`, `switch-count`, `watchdog-ms`,
/// `pwm-period-ms`); the device thread starts when the first workload binds,
/// and reconnects with backoff if the controller is unplugged.
///
/// # Examples
///
/// ```no_run
/// use wash_runtime::plugin::wpf_hardware::P3Roc;
///
/// let _plugin = P3Roc::builder().switch_count(64).build();
/// ```
#[derive(bon::Builder)]
pub struct P3Roc {
    /// FTDI serial number to bind to (first P-ROC-family device if unset).
    pub serial: Option<String>,
    /// Installed switch count (initial notify-rule sweep + state bitmap size).
    #[builder(default = 64)]
    pub switch_count: u16,
    /// Firmware watchdog reset time in ms; 0 disables it.
    #[builder(default = 1000)]
    pub watchdog_ms: u16,
    /// PWM window for `hold-coil` duty cycles, in ms (max 127).
    #[builder(default = 10)]
    pub pwm_period_ms: u8,
    #[builder(skip)]
    state: std::sync::Mutex<PluginState>,
}

#[derive(Default)]
struct PluginState {
    overrides: HashMap<String, String>,
    device: Option<std::sync::Arc<DeviceShared>>,
    /// Per-workload event buffers for `poll-events`. Created lazily on the
    /// first poll; the device thread fans events out to every subscriber
    /// (bounded channel — a consumer that stops polling loses oldest events
    /// and re-syncs via `read-switch-state`).
    subscribers: HashMap<String, tokio::sync::mpsc::Receiver<HwEvent>>,
}

impl P3Roc {
    /// Resolved device config: builder values overlaid with interface config.
    fn device_config(&self, overrides: &HashMap<String, String>) -> anyhow::Result<DeviceConfig> {
        let mut config = DeviceConfig {
            serial: self.serial.clone(),
            switch_count: self.switch_count,
            watchdog_ms: self.watchdog_ms,
        };
        for (key, value) in overrides {
            match key.as_str() {
                "serial" => config.serial = Some(value.clone()),
                "switch-count" => {
                    config.switch_count = value.parse().map_err(|e| {
                        anyhow::anyhow!("invalid 'switch-count' value '{value}': {e}")
                    })?;
                }
                "watchdog-ms" => {
                    config.watchdog_ms = value.parse().map_err(|e| {
                        anyhow::anyhow!("invalid 'watchdog-ms' value '{value}': {e}")
                    })?;
                }
                "pwm-period-ms" => {} // read separately in pwm_period()
                other => {
                    tracing::debug!(key = other, "ignoring unrecognized wpf:hardware config key");
                }
            }
        }
        anyhow::ensure!(
            config.switch_count > 0
                && config.switch_count <= 256
                && config.switch_count.is_multiple_of(32),
            "'switch-count' must be a multiple of 32 between 32 and 256, got {}",
            config.switch_count
        );
        Ok(config)
    }

    fn pwm_period(&self) -> u8 {
        let state = super::lock_unpoisoned(&self.state);
        state
            .overrides
            .get("pwm-period-ms")
            .and_then(|v| v.parse().ok())
            .unwrap_or(self.pwm_period_ms)
            .clamp(2, 127)
    }

    /// Returns the running device handle, starting the IO thread on first use.
    fn device(&self) -> anyhow::Result<std::sync::Arc<DeviceShared>> {
        let mut state = super::lock_unpoisoned(&self.state);
        if let Some(device) = &state.device {
            return Ok(std::sync::Arc::clone(device));
        }
        let config = self.device_config(&state.overrides)?;
        tracing::info!(?config, "starting P3-ROC device thread");
        let device = std::sync::Arc::new(DeviceShared::spawn(config)?);
        state.device = Some(std::sync::Arc::clone(&device));
        Ok(device)
    }

    /// Sends a pre-encoded write burst and awaits FIFO acceptance.
    async fn write(&self, words: Vec<u32>) -> Result<(), FastError> {
        let device = self.device().map_err(config_err)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        device
            .cmd_tx
            .send(HwCommand::Write(words, tx))
            .map_err(|_| FastError::NotConnected)?;
        rx.await
            .map_err(|_| FastError::NotConnected)?
            .map_err(Into::into)
    }

    /// Reads `words` registers starting at `addr` of module `select`.
    async fn read(&self, select: u32, addr: u32, words: u32) -> Result<Vec<u32>, FastError> {
        let device = self.device().map_err(config_err)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        device
            .cmd_tx
            .send(HwCommand::Read {
                select,
                addr,
                words,
                resp: tx,
            })
            .map_err(|_| FastError::NotConnected)?;
        rx.await
            .map_err(|_| FastError::NotConnected)?
            .map_err(Into::into)
    }
}

fn config_err(e: anyhow::Error) -> FastError {
    FastError::Protocol(format!("invalid wpf:hardware configuration: {e}"))
}

impl bindings::wpf::hardware::controller::Host for ActiveCtx<'_> {
    async fn pulse_coil(
        &mut self,
        coil_id: u8,
        duration_ms: u16,
    ) -> wasmtime::Result<Result<(), FastError>> {
        let plugin = self.try_get_plugin::<P3Roc>(WPF_HARDWARE_ID)?;
        // The driver config table's drive-time field is 8 bits of ms.
        let duration = u8::try_from(duration_ms).unwrap_or_else(|_| {
            tracing::debug!(duration_ms, "pulse duration clamped to 255 ms");
            u8::MAX
        });
        if duration == 0 {
            return Ok(Ok(()));
        }
        Ok(plugin
            .write(proto::driver_update(coil_id, DriverState::pulse(duration)))
            .await)
    }

    async fn hold_coil(&mut self, coil_id: u8, pwm: u8) -> wasmtime::Result<Result<(), FastError>> {
        let plugin = self.try_get_plugin::<P3Roc>(WPF_HARDWARE_ID)?;
        let state = match pwm {
            0 => DriverState::off(),
            u8::MAX => DriverState::hold(),
            duty => {
                let period = u16::from(plugin.pwm_period());
                let on = ((u16::from(duty) * period + 127) / 255).clamp(1, period - 1);
                DriverState::patter(on as u8, (period - on) as u8)
            }
        };
        Ok(plugin.write(proto::driver_update(coil_id, state)).await)
    }

    async fn release_coil(&mut self, coil_id: u8) -> wasmtime::Result<Result<(), FastError>> {
        let plugin = self.try_get_plugin::<P3Roc>(WPF_HARDWARE_ID)?;
        Ok(plugin
            .write(proto::driver_update(coil_id, DriverState::off()))
            .await)
    }

    async fn configure_autofire(
        &mut self,
        switch_id: u8,
        coil_id: u8,
        pulse_ms: u16,
        cooldown_ms: u16,
    ) -> wasmtime::Result<Result<(), FastError>> {
        let plugin = self.try_get_plugin::<P3Roc>(WPF_HARDWARE_ID)?;
        let pulse = u8::try_from(pulse_ms).unwrap_or(u8::MAX);
        let rule = SwitchRule {
            switch_num: switch_id,
            transition: SwitchTransition::ClosedNondebounced,
            // The debounced notify rule written at init still reports this
            // switch to the host; the autofire rule itself stays quiet.
            notify_host: false,
            reload_active: cooldown_ms > 0,
            driver: if pulse > 0 {
                Some((coil_id, DriverState::pulse(pulse)))
            } else {
                // pulse-ms 0 clears the autofire rule.
                None
            },
        };
        Ok(plugin.write(proto::switch_rule_update(&rule)).await)
    }

    async fn read_switch_state(&mut self) -> wasmtime::Result<Result<Vec<u8>, FastError>> {
        let plugin = self.try_get_plugin::<P3Roc>(WPF_HARDWARE_ID)?;
        let switch_count = {
            let state = crate::plugin::wpf_hardware::lock_unpoisoned(&plugin.state);
            match plugin.device_config(&state.overrides) {
                Ok(config) => config.switch_count,
                Err(e) => return Ok(Err(config_err(e))),
            }
        };
        let num_words = u32::from(switch_count / 32);
        let words = match plugin
            .read(
                proto::SWITCH_CTRL_SELECT,
                P3_ROC_SWITCH_STATE_BASE_ADDR,
                num_words,
            )
            .await
        {
            Ok(words) => words,
            Err(e) => return Ok(Err(e)),
        };
        if words.len() != num_words as usize {
            return Ok(Err(FastError::Protocol(format!(
                "switch state read returned {} words, expected {num_words}",
                words.len()
            ))));
        }
        // Bitmap: switch n = bit n%8 of byte n/8 (LSB-first, matching the
        // FPGA's LSB-first switch numbering within each state word). Each
        // 32-bit state word is exactly 4 bitmap bytes.
        let bitmap: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        Ok(Ok(bitmap))
    }

    async fn send_raw(&mut self, payload: Vec<u8>) -> wasmtime::Result<Result<Vec<u8>, FastError>> {
        let plugin = self.try_get_plugin::<P3Roc>(WPF_HARDWARE_ID)?;
        if payload.is_empty() || !payload.len().is_multiple_of(4) {
            return Ok(Err(FastError::Protocol(format!(
                "raw payload must be a non-empty multiple of 4 bytes (32-bit \
                 big-endian words), got {} bytes",
                payload.len()
            ))));
        }
        let words: Vec<u32> = payload
            .chunks_exact(4)
            .map(|c| u32::from_be_bytes(c.try_into().unwrap_or_default()))
            .collect();
        // A single read-request word is serviced as a real register read and
        // the response words are returned big-endian — this is how diagnostics
        // (e.g. the verify-p3roc example) fetch chip id / firmware version
        // without a dedicated WIT surface. Anything else is written verbatim.
        if let &[word] = words.as_slice()
            && let Some((select, addr, num_words)) = proto::parse_read_request(word)
        {
            if num_words > 64 {
                return Ok(Err(FastError::Protocol(format!(
                    "raw read of {num_words} words exceeds the 64-word diagnostics limit"
                ))));
            }
            return Ok(plugin
                .read(select, addr, num_words)
                .await
                .map(|words| proto::words_to_bytes(&words)));
        }
        Ok(plugin.write(words).await.map(|_| Vec::new()))
    }

    async fn poll_events(
        &mut self,
        max_events: u32,
    ) -> wasmtime::Result<Result<Vec<FastEvent>, FastError>> {
        let plugin = self.try_get_plugin::<P3Roc>(WPF_HARDWARE_ID)?;
        let workload_id = self.workload_id.to_string();

        let mut state = crate::plugin::wpf_hardware::lock_unpoisoned(&plugin.state);
        // Lazily create this workload's subscriber on first poll (starting
        // the device thread if needed). Holding the state lock here is fine:
        // polls are µs-scale drains, and the device thread never takes it.
        let state = &mut *state;
        let rx = match state.subscribers.entry(workload_id.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let device = if let Some(device) = &state.device {
                    std::sync::Arc::clone(device)
                } else {
                    let config = match plugin.device_config(&state.overrides) {
                        Ok(config) => config,
                        Err(e) => return Ok(Err(config_err(e))),
                    };
                    tracing::info!(?config, "starting P3-ROC device thread");
                    let device = match DeviceShared::spawn(config) {
                        Ok(device) => std::sync::Arc::new(device),
                        Err(e) => return Ok(Err(config_err(e))),
                    };
                    state.device = Some(std::sync::Arc::clone(&device));
                    device
                };
                entry.insert(device.subscribe())
            }
        };
        let max = (max_events as usize).min(1024);
        let mut out = Vec::new();
        while out.len() < max {
            match rx.try_recv() {
                Ok(event) => out.push(event.into()),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    // Channel closed (device thread restart or backpressure
                    // cutoff): drop the stale subscriber; the next poll
                    // re-subscribes and the consumer re-syncs via
                    // read-switch-state.
                    state.subscribers.remove(&workload_id);
                    break;
                }
            }
        }
        Ok(Ok(out))
    }
}

#[async_trait::async_trait]
impl HostPlugin for P3Roc {
    fn id(&self) -> &'static str {
        WPF_HARDWARE_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: HashSet::from([WitInterface::from(WPF_HARDWARE_INTERFACE)]),
            ..Default::default()
        }
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        let Some(interface) = interfaces.get("wpf", "hardware", &["controller"]) else {
            tracing::warn!(
                "P3Roc plugin requested for non-wpf:hardware/controller interface(s): {:?}",
                interfaces
            );
            return Ok(());
        };

        if !interface.config.is_empty() {
            let mut state = super::lock_unpoisoned(&self.state);
            if state.device.is_some() && state.overrides != interface.config {
                tracing::warn!(
                    "P3-ROC device thread already running; new wpf:hardware interface \
                     config will apply after host restart"
                );
            } else {
                state.overrides = interface.config.clone();
                // Validate early so a bad manifest fails the bind, not the
                // first coil pulse.
                self.device_config(&state.overrides)?;
            }
        }

        bindings::wpf::hardware::controller::add_to_linker::<_, SharedCtx>(
            item.linker(),
            extract_active_ctx,
        )?;

        tracing::debug!(
            workload_id = item.workload_id(),
            "P3Roc plugin bound to workload"
        );
        Ok(())
    }

    async fn on_workload_unbind(
        &self,
        workload_id: &str,
        _interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        let mut state = super::lock_unpoisoned(&self.state);
        state.subscribers.remove(workload_id);
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let (device, subscribers) = {
            let mut state = super::lock_unpoisoned(&self.state);
            (state.device.take(), std::mem::take(&mut state.subscribers))
        };
        drop(subscribers);
        if let Some(device) = device {
            device.stop();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_plugin() -> P3Roc {
        P3Roc::builder().build()
    }

    #[test]
    fn interface_config_overrides_builder_defaults() {
        let plugin = test_plugin();
        let overrides = HashMap::from([
            ("serial".to_string(), "PR000123".to_string()),
            ("switch-count".to_string(), "128".to_string()),
            ("watchdog-ms".to_string(), "500".to_string()),
        ]);
        let config = plugin.device_config(&overrides).unwrap();
        assert_eq!(config.serial.as_deref(), Some("PR000123"));
        assert_eq!(config.switch_count, 128);
        assert_eq!(config.watchdog_ms, 500);
    }

    #[test]
    fn rejects_bad_switch_count() {
        let plugin = test_plugin();
        for bad in ["0", "33", "512"] {
            let overrides = HashMap::from([("switch-count".to_string(), bad.to_string())]);
            assert!(plugin.device_config(&overrides).is_err(), "{bad}");
        }
    }

    #[test]
    fn pwm_period_reads_override_and_clamps() {
        let plugin = test_plugin();
        assert_eq!(plugin.pwm_period(), 10);
        plugin
            .state
            .lock()
            .unwrap()
            .overrides
            .insert("pwm-period-ms".to_string(), "200".to_string());
        assert_eq!(plugin.pwm_period(), 127);
    }
}
