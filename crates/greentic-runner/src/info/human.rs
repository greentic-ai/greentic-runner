//! Human-readable rendering of [`super::InfoReport`].
//!
//! The output is terminal-oriented: fixed-width label columns, one blank line
//! between sections, no colour. CLI wiring (E5) will select between this and
//! `serde_json::to_string_pretty` based on `--format`.

use super::report::InfoReport;
use std::fmt::Write;

pub fn render(r: &InfoReport) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "greentic-runner {}", r.runner_version);
    let _ = writeln!(s, "Wasmtime      {}", r.wasmtime_version);
    let _ = writeln!(s, "Target        {}", r.target_triple);
    let build_line = match &r.git_sha {
        Some(sha) => format!(
            "{} {} {} {} {}",
            r.build_profile, "\u{00B7}", r.build_timestamp_utc, "\u{00B7}", sha
        ),
        None => format!(
            "{} {} {}",
            r.build_profile, "\u{00B7}", r.build_timestamp_utc
        ),
    };
    let _ = writeln!(s, "Build         {build_line}");

    let _ = writeln!(
        s,
        "\nPack format versions   {}",
        r.pack_format_versions
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let enabled = if r.features.enabled.is_empty() {
        "(none)".to_string()
    } else {
        r.features.enabled.join(", ")
    };
    let disabled = if r.features.disabled.is_empty() {
        "(none)".to_string()
    } else {
        r.features.disabled.join(", ")
    };
    let _ = writeln!(s, "Enabled features       {enabled}");
    let _ = writeln!(s, "Disabled features      {disabled}");

    let _ = writeln!(s, "\nWASI imports (host-provided)");
    for b in &r.wasi_imports {
        let v = b.version.as_deref().unwrap_or("");
        let _ = writeln!(s, "  {:<22} {v}", b.interface);
    }

    let _ = writeln!(s, "\nGreentic imports (host-provided)");
    for b in &r.greentic_imports {
        let v = if b.versions.is_empty() {
            b.version.clone().unwrap_or_default()
        } else {
            b.versions.join(", ")
        };
        let extra = if b.opt_in_per_pack {
            " (opt-in per pack)"
        } else {
            ""
        };
        let _ = writeln!(s, "  {:<45} {v}{extra}", b.interface);
    }

    s
}

#[cfg(test)]
mod tests {
    use super::super::report::collect;
    use super::*;

    #[test]
    fn renders_real_collect() {
        let out = render(&collect());
        assert!(out.contains("greentic-runner "));
        assert!(out.contains("Wasmtime "));
        assert!(out.contains("WASI imports"));
        assert!(out.contains("Greentic imports"));
        assert!(out.contains("Pack format versions"));
    }

    #[test]
    fn renders_build_without_git_sha() {
        let mut r = collect();
        r.git_sha = None;
        let out = render(&r);
        // Absent sha must not produce consecutive middle-dots.
        assert!(!out.contains("\u{00B7} \u{00B7}"));
    }
}
