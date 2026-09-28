//! Pinball hardware plugin for WebAssembly components.
//!
//! Implements `wpf:hardware/controller@0.2.0` — the coil/switch/autofire
//! interface of the wasm-pinball-framework — against a Multimorphic P3-ROC
//! controller attached over USB (FTDI FIFO). The framework's `device-manager`
//! component imports this interface; on hosts with this plugin registered its
//! commands land on the real machine.
//!
//! This is the same seam the FAST controller occupies in the framework (the
//! `wpf/plugins/fast-controller` sidecar); the P3-ROC variant is the first to
//! run in-host. The interface is wasip3-native: commands are `async func`s and
//! switch events arrive on the `stream<fast-event>` returned by
//! `subscribe-events`.
//!
//! # Configuration
//!
//! Defaults come from the [`P3Roc`] builder; workloads can override them via
//! `wpf:hardware` interface config (flat keys):
//!
//! ```text
//! serial = PR3000123      # FTDI serial; first P-ROC-family device if unset
//! switch-count = 64       # installed switches (multiple of 32, max 256)
//! watchdog-ms = 1000      # firmware watchdog; 0 disables
//! pwm-period-ms = 10      # hold-coil PWM window (2-127 ms)
//! ```
//!
//! The device thread starts when the first workload binds and reconnects with
//! backoff when the controller is unplugged, reporting `link-down`/`link-up`
//! on the event stream.
//!
//! # Protocol
//!
//! The wire protocol (32-bit big-endian words over an FT245/FT240 FIFO) is
//! transcribed from Multimorphic's MIT-licensed libpinproc; see
//! [`proto`] for the subset in use and the cross-checked test vectors.

mod device;
mod ftdi;
mod p3roc;
pub mod proto;

pub use device::{DeviceConfig, HwError, HwEvent};
pub use p3roc::P3Roc;

const WPF_HARDWARE_ID: &str = "wpf-hardware-p3roc";
const WPF_HARDWARE_INTERFACE: &str = "wpf:hardware/controller@0.2.0";

/// Locks a mutex, recovering the guard if a previous holder panicked. All
/// state behind these locks (subscriber lists, plugin config) stays
/// internally consistent across a panic, so continuing is safe.
fn lock_unpoisoned<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
