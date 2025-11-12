use std::fs;
use std::path::{Path, PathBuf};

use greentic_runner::gen_bindings::{self, GeneratorOptions};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests/fixtures/gen-bindings")
        .join(name)
}

fn generate_serialized(path: &Path, opts: GeneratorOptions) -> String {
    let metadata = gen_bindings::load_pack(path).expect("load pack");
    let bindings = gen_bindings::generate_bindings(&metadata, opts).expect("generate bindings");
    serde_yaml::to_string(&bindings).expect("serialize")
}

#[test]
fn complete_binding_matches_golden() {
    let dir = fixture("weather-demo");
    let yaml = generate_serialized(
        &dir,
        GeneratorOptions {
            complete: true,
            ..Default::default()
        },
    );
    let golden =
        fs::read_to_string(dir.join("bindings.complete.yaml")).expect("read golden complete");
    assert_eq!(golden, yaml);
}

#[test]
fn strict_binding_matches_golden() {
    let dir = fixture("weather-demo-strict");
    let yaml = generate_serialized(
        &dir,
        GeneratorOptions {
            complete: true,
            strict: true,
            ..Default::default()
        },
    );
    let golden = fs::read_to_string(dir.join("bindings.strict.yaml")).expect("read golden strict");
    assert_eq!(golden, yaml);
}
