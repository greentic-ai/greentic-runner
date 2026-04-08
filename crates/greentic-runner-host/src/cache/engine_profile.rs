use sha2::{Digest, Sha256};
use wasmtime::Engine;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuPolicy {
    Native,
    Baseline,
}

impl CpuPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            CpuPolicy::Native => "native",
            CpuPolicy::Baseline => "baseline",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineProfile {
    pub wasmtime_version: String,
    pub target_triple: String,
    pub cpu_policy: CpuPolicy,
    pub config_fingerprint: String,
    pub engine_profile_id: String,
}

impl EngineProfile {
    pub fn from_engine(
        _engine: &Engine,
        cpu_policy: CpuPolicy,
        config_fingerprint: String,
    ) -> Self {
        let wasmtime_version = wasmtime_environ::VERSION.to_string();
        let target_triple = format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS);
        let engine_profile_id = compute_engine_profile_id(
            &wasmtime_version,
            &target_triple,
            cpu_policy,
            &config_fingerprint,
        );
        Self {
            wasmtime_version,
            target_triple,
            cpu_policy,
            config_fingerprint,
            engine_profile_id,
        }
    }

    pub fn id(&self) -> &str {
        &self.engine_profile_id
    }
}

fn compute_engine_profile_id(
    wasmtime_version: &str,
    target_triple: &str,
    cpu_policy: CpuPolicy,
    config_fingerprint: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(wasmtime_version.as_bytes());
    hasher.update(target_triple.as_bytes());
    hasher.update(cpu_policy.as_str().as_bytes());
    hasher.update(config_fingerprint.as_bytes());
    format!("sha256:{}", to_hex(&hasher.finalize()))
}

fn to_hex(digest: &[u8]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_profile_id_is_stable() {
        let engine = Engine::default();
        let profile = EngineProfile::from_engine(&engine, CpuPolicy::Native, "default".to_string());
        let profile_again =
            EngineProfile::from_engine(&engine, CpuPolicy::Native, "default".to_string());
        assert_eq!(profile.id(), profile_again.id());
    }
}
