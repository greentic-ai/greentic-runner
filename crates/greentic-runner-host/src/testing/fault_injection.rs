use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum FaultPoint {
    BeforeComponentCall,
    AfterComponentCall,
    BeforeToolCall,
    AfterToolCall,
    StateRead,
    StateWrite,
    TemplateRender,
    PackResolve,
    Timeout,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum FaultErrorKind {
    Transient,
    Permanent,
    Timeout,
    Trap,
    BadInput,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum FaultMode {
    Always,
    Once,
    Nth(u32),
    Rate(u32),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FaultSpec {
    pub point: FaultPoint,
    pub mode: FaultMode,
    pub error_kind: FaultErrorKind,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct FaultContext<'a> {
    pub pack_id: &'a str,
    pub flow_id: &'a str,
    pub node_id: Option<&'a str>,
    pub attempt: u32,
}

#[derive(Clone, Debug)]
pub struct FaultInjector {
    specs: Vec<FaultSpec>,
    pack_id: Option<String>,
    flow_id: Option<String>,
    seed: u64,
    counters: HashMap<usize, u32>,
    max_attempt: u32,
    injected: u32,
}

#[derive(Clone, Debug, Default)]
pub struct FaultStats {
    pub max_attempt: u32,
    pub injected: u32,
}

#[derive(Clone, Debug)]
pub struct InjectedError {
    pub kind: FaultErrorKind,
    pub message: String,
}

impl fmt::Display for InjectedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match self.kind {
            FaultErrorKind::Transient => "transient",
            FaultErrorKind::Permanent => "permanent dlq_recommended",
            FaultErrorKind::Timeout => "timeout",
            FaultErrorKind::Trap => "wasm_trap",
            FaultErrorKind::BadInput => "bad_input",
        };
        write!(f, "{}: {}", prefix, self.message)
    }
}

impl std::error::Error for InjectedError {}

static INJECTOR: Lazy<Mutex<Option<Arc<Mutex<FaultInjector>>>>> = Lazy::new(|| Mutex::new(None));

pub fn set_injector(injector: FaultInjector) {
    let mut guard = INJECTOR.lock();
    *guard = Some(Arc::new(Mutex::new(injector)));
}

pub fn clear_injector() {
    let mut guard = INJECTOR.lock();
    *guard = None;
}

pub fn stats() -> FaultStats {
    let guard = INJECTOR.lock();
    guard
        .as_ref()
        .map(|injector| {
            let injector = injector.lock();
            FaultStats {
                max_attempt: injector.max_attempt,
                injected: injector.injected,
            }
        })
        .unwrap_or_default()
}

pub fn maybe_fail(point: FaultPoint, ctx: FaultContext<'_>) -> Result<(), InjectedError> {
    let injector = {
        let guard = INJECTOR.lock();
        guard.as_ref().map(Arc::clone)
    };
    let Some(injector) = injector else {
        return Ok(());
    };
    let mut injector = injector.lock();
    injector.maybe_fail(point, ctx)
}

impl FaultInjector {
    pub fn new(specs: Vec<FaultSpec>) -> Self {
        Self {
            specs,
            pack_id: None,
            flow_id: None,
            seed: 0,
            counters: HashMap::new(),
            max_attempt: 0,
            injected: 0,
        }
    }

    pub fn with_pack_id(mut self, pack_id: impl Into<String>) -> Self {
        self.pack_id = Some(pack_id.into());
        self
    }

    pub fn with_flow_id(mut self, flow_id: impl Into<String>) -> Self {
        self.flow_id = Some(flow_id.into());
        self
    }

    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    fn maybe_fail(
        &mut self,
        point: FaultPoint,
        ctx: FaultContext<'_>,
    ) -> Result<(), InjectedError> {
        if let Some(pack_id) = &self.pack_id
            && pack_id != ctx.pack_id
        {
            return Ok(());
        }
        if let Some(flow_id) = &self.flow_id
            && flow_id != ctx.flow_id
        {
            return Ok(());
        }
        if ctx.attempt > self.max_attempt {
            self.max_attempt = ctx.attempt;
        }

        for (idx, spec) in self.specs.iter().enumerate() {
            if spec.point != point {
                continue;
            }
            let count = self.counters.entry(idx).or_insert(0);
            *count += 1;
            if !should_inject(spec.mode, *count, self.seed) {
                continue;
            }
            self.injected += 1;
            return Err(InjectedError {
                kind: spec.error_kind,
                message: spec.message.clone(),
            });
        }
        Ok(())
    }
}

fn should_inject(mode: FaultMode, count: u32, seed: u64) -> bool {
    match mode {
        FaultMode::Always => true,
        FaultMode::Once => count == 1,
        FaultMode::Nth(n) => count == n.max(1),
        FaultMode::Rate(rate) => {
            let rate = rate.min(100);
            let bucket = (seed.wrapping_add(count as u64) % 100) as u32;
            bucket < rate
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn ctx<'a>(pack: &'a str, flow: &'a str, attempt: u32) -> FaultContext<'a> {
        FaultContext {
            pack_id: pack,
            flow_id: flow,
            node_id: None,
            attempt,
        }
    }

    fn spec(point: FaultPoint, mode: FaultMode, kind: FaultErrorKind) -> FaultSpec {
        FaultSpec {
            point,
            mode,
            error_kind: kind,
            message: "boom".into(),
        }
    }

    #[test]
    fn should_inject_always_fires_every_time() {
        assert!(should_inject(FaultMode::Always, 1, 0));
        assert!(should_inject(FaultMode::Always, 99, 7));
    }

    #[test]
    fn should_inject_once_fires_only_on_first_count() {
        assert!(should_inject(FaultMode::Once, 1, 0));
        assert!(!should_inject(FaultMode::Once, 2, 0));
        assert!(!should_inject(FaultMode::Once, 0, 0));
    }

    #[test]
    fn should_inject_nth_fires_on_target_count() {
        assert!(!should_inject(FaultMode::Nth(3), 2, 0));
        assert!(should_inject(FaultMode::Nth(3), 3, 0));
        assert!(!should_inject(FaultMode::Nth(3), 4, 0));
        // n=0 is treated as 1
        assert!(should_inject(FaultMode::Nth(0), 1, 0));
    }

    #[test]
    fn should_inject_rate_clamps_above_100_and_uses_seed() {
        // Rate 100 -> always fires (clamped)
        assert!(should_inject(FaultMode::Rate(150), 0, 0));
        // Rate 0 -> never fires
        assert!(!should_inject(FaultMode::Rate(0), 0, 0));
        // Seed determinism: bucket = (seed + count) % 100
        // seed=10, count=5 -> 15 < 50 -> true
        assert!(should_inject(FaultMode::Rate(50), 5, 10));
        // seed=80, count=5 -> 85 < 50 -> false
        assert!(!should_inject(FaultMode::Rate(50), 5, 80));
    }

    #[test]
    fn injected_error_display_uses_kind_prefix() {
        for (kind, prefix) in [
            (FaultErrorKind::Transient, "transient"),
            (FaultErrorKind::Permanent, "permanent dlq_recommended"),
            (FaultErrorKind::Timeout, "timeout"),
            (FaultErrorKind::Trap, "wasm_trap"),
            (FaultErrorKind::BadInput, "bad_input"),
        ] {
            let err = InjectedError {
                kind,
                message: "msg".into(),
            };
            assert_eq!(err.to_string(), format!("{prefix}: msg"));
        }
    }

    #[test]
    fn maybe_fail_returns_ok_when_pack_filter_does_not_match() {
        let mut injector = FaultInjector::new(vec![spec(
            FaultPoint::BeforeComponentCall,
            FaultMode::Always,
            FaultErrorKind::Transient,
        )])
        .with_pack_id("expected-pack");
        let res = injector.maybe_fail(FaultPoint::BeforeComponentCall, ctx("other-pack", "f", 1));
        assert!(res.is_ok());
    }

    #[test]
    fn maybe_fail_returns_ok_when_flow_filter_does_not_match() {
        let mut injector = FaultInjector::new(vec![spec(
            FaultPoint::BeforeComponentCall,
            FaultMode::Always,
            FaultErrorKind::Transient,
        )])
        .with_flow_id("expected-flow");
        let res = injector.maybe_fail(FaultPoint::BeforeComponentCall, ctx("p", "other", 1));
        assert!(res.is_ok());
    }

    #[test]
    fn maybe_fail_returns_ok_when_no_specs_match_point() {
        let mut injector = FaultInjector::new(vec![spec(
            FaultPoint::AfterComponentCall,
            FaultMode::Always,
            FaultErrorKind::Transient,
        )]);
        let res = injector.maybe_fail(FaultPoint::BeforeComponentCall, ctx("p", "f", 1));
        assert!(res.is_ok());
    }

    #[test]
    fn maybe_fail_injects_once_then_passes() {
        let mut injector = FaultInjector::new(vec![spec(
            FaultPoint::StateWrite,
            FaultMode::Once,
            FaultErrorKind::Transient,
        )])
        .with_pack_id("p")
        .with_flow_id("f");
        // First call: counter=1 -> Once fires.
        let first = injector.maybe_fail(FaultPoint::StateWrite, ctx("p", "f", 1));
        assert!(first.is_err());
        let err = first.unwrap_err();
        assert_eq!(err.kind, FaultErrorKind::Transient);
        assert_eq!(err.message, "boom");
        // Second call: counter=2 -> Once does not fire.
        let second = injector.maybe_fail(FaultPoint::StateWrite, ctx("p", "f", 2));
        assert!(second.is_ok());
        assert_eq!(injector.injected, 1);
        assert_eq!(injector.max_attempt, 2);
    }

    #[test]
    fn maybe_fail_tracks_max_attempt_even_when_no_match() {
        let mut injector = FaultInjector::new(Vec::new());
        let _ = injector.maybe_fail(FaultPoint::Timeout, ctx("p", "f", 7));
        assert_eq!(injector.max_attempt, 7);
        let _ = injector.maybe_fail(FaultPoint::Timeout, ctx("p", "f", 3));
        // Should not regress
        assert_eq!(injector.max_attempt, 7);
    }

    #[test]
    fn with_seed_is_recorded() {
        let injector = FaultInjector::new(Vec::new()).with_seed(42);
        assert_eq!(injector.seed, 42);
    }

    #[test]
    #[serial]
    fn set_and_clear_global_injector_round_trip() {
        clear_injector();
        // No injector: maybe_fail is a no-op.
        assert!(maybe_fail(FaultPoint::PackResolve, ctx("p", "f", 1)).is_ok());
        // Empty stats when none set.
        let s = stats();
        assert_eq!(s.injected, 0);
        assert_eq!(s.max_attempt, 0);

        set_injector(
            FaultInjector::new(vec![spec(
                FaultPoint::PackResolve,
                FaultMode::Always,
                FaultErrorKind::Permanent,
            )])
            .with_pack_id("p"),
        );
        let res = maybe_fail(FaultPoint::PackResolve, ctx("p", "f", 1));
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert_eq!(err.kind, FaultErrorKind::Permanent);

        let s = stats();
        assert_eq!(s.injected, 1);
        assert_eq!(s.max_attempt, 1);

        clear_injector();
        // Now no-op again.
        assert!(maybe_fail(FaultPoint::PackResolve, ctx("p", "f", 9)).is_ok());
        let s = stats();
        assert_eq!(s.injected, 0);
        assert_eq!(s.max_attempt, 0);
    }
}
