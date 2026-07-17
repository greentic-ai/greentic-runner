use anyhow::Result;

use crate::TelemetryCfg;
use crate::http::health::HealthState;

use greentic_telemetry::init_telemetry_from_config;
use tracing::info;

/// Initialise host-level subsystems (telemetry, health markers).
pub fn init(health: &HealthState, telemetry: Option<&TelemetryCfg>) -> Result<()> {
    if let Some(cfg) = telemetry {
        info!(
            service = cfg.config.service_name,
            "initialising telemetry pipeline"
        );
        init_telemetry_from_config(cfg.config.clone(), cfg.export.clone())?;
    }

    health.set_ready();
    Ok(())
}
