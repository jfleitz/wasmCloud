//! Pinball event-bus plugin for WebAssembly components.
//!
//! Implements `wpf:core/events@0.1.0` — the cross-component event bus of the
//! wasm-pinball-framework — on top of the host's data NATS connection. Every
//! wpf game component imports this interface; on hosts with this plugin
//! registered, `events.post` publishes the event as one JSON message on a
//! NATS subject, where anything on the lattice (other workloads, diagnostics
//! tooling, `nats sub`) can observe it.
//!
//! # Wire format
//!
//! One event is one message on `<subject-prefix>.<event-name>` (event names
//! are sanitized to `[A-Za-z0-9_-]` so they can't alter the subject
//! structure). The body is lossless JSON of the WIT record, payload pairs
//! kept as ordered two-element arrays:
//!
//! ```text
//! wpf.events.ball_started → {"name":"ball_started","priority":0,
//!                            "payload":[["player","1"],["ball","1"]],
//!                            "origin":"ball-controller"}
//! ```
//!
//! # Configuration
//!
//! Workloads can override defaults via `wpf:core` interface config
//! (flat keys):
//!
//! ```text
//! subject-prefix = wpf.events   # NATS subject prefix for posted events
//! ```
//!
//! The prefix is plugin-global: if two workloads on one host configure
//! different prefixes, the last bind wins (a warning is logged). Run one
//! machine per host — the framework's deployment shape anyway.
//!
//! # Subscriptions
//!
//! `events.subscribe` records the MPF-style glob pattern and succeeds, but
//! host-side *delivery* into components needs an inbound path the 0.1.0 WIT
//! doesn't have (no handler export, no event stream — compare
//! `wpf:hardware/controller.subscribe-events`). Components that want bus
//! traffic today export `wasmcloud:messaging/handler` and subscribe to the
//! event subjects through the messaging plugin. When the WIT grows a
//! stream-returning revision, delivery lands here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tracing::{debug, instrument, warn};

use crate::engine::ctx::{ActiveCtx, SharedCtx, extract_active_ctx};
use crate::engine::workload::WorkloadItem;
use crate::plugin::{HostPlugin, WitInterfaces};
use crate::wit::{WitInterface, WitWorld};

const WPF_CORE_EVENTS_ID: &str = "wpf-core-events-nats";
const WPF_CORE_EVENTS_INTERFACE: &str = "wpf:core/events@0.1.0";
const DEFAULT_SUBJECT_PREFIX: &str = "wpf.events";

mod bindings {
    wasmtime::component::bindgen!({
        world: "wpf-core-events",
        imports: { default: async | trappable | tracing },
    });
}

use bindings::wpf::core::events::{Event, Host};

/// The NATS-backed `wpf:core/events` bus (`wpf:core/events@0.1.0`).
///
/// One plugin instance per host, sharing the host's data NATS client. Events
/// posted by any bound component are published fire-and-forget; a NATS
/// publish failure is logged and dropped rather than trapping the component —
/// a pinball machine must keep playing through a flaky bus.
pub struct CoreEvents {
    client: Arc<async_nats::Client>,
    state: std::sync::Mutex<PluginState>,
}

#[derive(Default)]
struct PluginState {
    overrides: HashMap<String, String>,
}

impl CoreEvents {
    /// Creates the plugin around the host's (data-plane) NATS client.
    pub fn new(client: Arc<async_nats::Client>) -> Self {
        Self {
            client,
            state: std::sync::Mutex::new(PluginState::default()),
        }
    }

    /// Resolved subject prefix: interface config overlaid on the default.
    fn subject_prefix(&self) -> String {
        let state = lock_unpoisoned(&self.state);
        state
            .overrides
            .get("subject-prefix")
            .cloned()
            .unwrap_or_else(|| DEFAULT_SUBJECT_PREFIX.to_string())
    }
}

/// Locks a mutex, recovering the guard if a previous holder panicked. The
/// only state behind this lock is the override map, which stays internally
/// consistent across a panic, so continuing is safe.
fn lock_unpoisoned<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Restricts an event name to a single NATS subject token: `[A-Za-z0-9_-]`
/// pass through, anything else (dots, wildcards, spaces) becomes `_` so a
/// posted name can't extend or wildcard the subject hierarchy.
fn sanitize_token(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "_".to_string()
    } else {
        sanitized
    }
}

fn subject_for(prefix: &str, event_name: &str) -> String {
    format!("{prefix}.{}", sanitize_token(event_name))
}

/// Lossless JSON encoding of the WIT event record; payload pairs stay ordered
/// `[key, value]` arrays (duplicates allowed, exactly as posted).
fn encode_event(ev: &Event) -> serde_json::Value {
    serde_json::json!({
        "name": ev.name,
        "priority": ev.priority,
        "payload": ev.payload,
        "origin": ev.origin,
    })
}

impl Host for ActiveCtx<'_> {
    #[instrument(name = "wpf.core.events.post", skip_all, fields(event = %ev.name, origin = %ev.origin))]
    async fn post(&mut self, ev: Event) -> wasmtime::Result<()> {
        let plugin = self.try_get_plugin::<CoreEvents>(WPF_CORE_EVENTS_ID)?;
        let subject = subject_for(&plugin.subject_prefix(), &ev.name);
        let body = serde_json::to_vec(&encode_event(&ev)).unwrap_or_default();
        // Fire-and-forget bus semantics: `post` has no error surface in the
        // WIT, and trapping the component over a transient NATS hiccup would
        // stop the game. Log and drop instead.
        if let Err(e) = plugin.client.publish(subject.clone(), body.into()).await {
            warn!(%subject, error = %e, "failed to publish wpf event");
        }
        Ok(())
    }

    #[instrument(name = "wpf.core.events.subscribe", skip_all, fields(pattern = %pattern))]
    async fn subscribe(&mut self, pattern: String) -> wasmtime::Result<Result<(), String>> {
        if pattern.is_empty() {
            return Ok(Err("subscribe pattern must not be empty".to_string()));
        }
        // Registration only — see the module docs: 0.1.0 has no inbound path
        // to deliver matches into the component, so the pattern is accepted
        // and recorded for the day the WIT grows one.
        debug!(
            component_id = %self.component_id,
            workload_id = %self.workload_id,
            "recorded wpf event subscription (delivery pending a stream-based WIT revision)"
        );
        Ok(Ok(()))
    }
}

#[async_trait::async_trait]
impl HostPlugin for CoreEvents {
    fn id(&self) -> &'static str {
        WPF_CORE_EVENTS_ID
    }

    fn world(&self) -> WitWorld {
        WitWorld {
            imports: HashSet::from([WitInterface::from(WPF_CORE_EVENTS_INTERFACE)]),
            ..Default::default()
        }
    }

    async fn on_workload_item_bind<'a>(
        &self,
        item: &mut WorkloadItem<'a>,
        interfaces: WitInterfaces<'_>,
    ) -> anyhow::Result<()> {
        let Some(interface) = interfaces.get("wpf", "core", &["events"]) else {
            warn!(
                "CoreEvents plugin requested for non-wpf:core/events interface(s): {:?}",
                interfaces
            );
            return Ok(());
        };

        if !interface.config.is_empty() {
            let mut state = lock_unpoisoned(&self.state);
            if !state.overrides.is_empty() && state.overrides != interface.config {
                warn!(
                    "wpf:core/events interface config differs from an earlier workload's; \
                     the new config wins for all workloads on this host"
                );
            }
            for key in interface.config.keys() {
                if key != "subject-prefix" {
                    debug!(key, "ignoring unrecognized wpf:core/events config key");
                }
            }
            state.overrides = interface.config.clone();
        }

        bindings::wpf::core::events::add_to_linker::<_, SharedCtx>(
            item.linker(),
            extract_active_ctx,
        )?;

        debug!(
            workload_id = item.workload_id(),
            "CoreEvents plugin bound to workload"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Pure-data seams only — subject shaping, config precedence, and the
    //! wire encoding. Anything needing a NATS server or the engine belongs
    //! in the integration suite.
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn sanitizes_event_names_into_one_subject_token() {
        assert_eq!(sanitize_token("ball_started"), "ball_started");
        assert_eq!(sanitize_token("mode.base.started"), "mode_base_started");
        assert_eq!(sanitize_token("evil.>"), "evil__");
        assert_eq!(sanitize_token("a b*c"), "a_b_c");
        assert_eq!(sanitize_token(""), "_");
    }

    #[test]
    fn subject_is_prefix_dot_name() {
        assert_eq!(
            subject_for("wpf.events", "s_left_flipper_active"),
            "wpf.events.s_left_flipper_active"
        );
        assert_eq!(subject_for("custom.prefix", "x.y"), "custom.prefix.x_y");
    }

    #[test]
    fn encodes_event_losslessly_with_ordered_pairs() {
        let ev = Event {
            name: "ball_started".to_string(),
            priority: 7,
            payload: vec![
                ("player".to_string(), "1".to_string()),
                ("player".to_string(), "dup-kept".to_string()),
            ],
            origin: "ball-controller".to_string(),
        };
        let json = serde_json::to_string(&encode_event(&ev)).unwrap();
        assert_eq!(
            json,
            r#"{"name":"ball_started","origin":"ball-controller","payload":[["player","1"],["player","dup-kept"]],"priority":7}"#
        );
    }
}
