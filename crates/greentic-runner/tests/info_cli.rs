use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_greentic-runner"))
}

#[test]
fn human_output_has_required_sections() {
    let out = bin().arg("info").output().expect("run info");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("greentic-runner "),
        "missing runner version line:\n{s}"
    );
    assert!(s.contains("Wasmtime "), "missing Wasmtime line:\n{s}");
    assert!(
        s.contains("Pack format versions"),
        "missing pack format line:\n{s}"
    );
    assert!(
        s.contains("WASI imports"),
        "missing WASI imports section:\n{s}"
    );
    assert!(
        s.contains("Greentic imports"),
        "missing Greentic imports section:\n{s}"
    );
}

#[test]
fn json_output_has_schema_version() {
    let out = bin()
        .args(["info", "--json"])
        .output()
        .expect("run info --json");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(v["info_schema_version"], 1);
    assert!(v["wasi_imports"].is_array());
    assert!(v["greentic_imports"].is_array());
    assert!(v["features"]["enabled"].is_array());
    assert!(v["features"]["disabled"].is_array());

    // Disjointness
    let enabled: Vec<String> = serde_json::from_value(v["features"]["enabled"].clone()).unwrap();
    let disabled: Vec<String> = serde_json::from_value(v["features"]["disabled"].clone()).unwrap();
    for f in &enabled {
        assert!(
            !disabled.contains(f),
            "feature {f} appears in both enabled and disabled"
        );
    }
}
