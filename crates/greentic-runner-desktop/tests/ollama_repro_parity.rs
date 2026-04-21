use std::path::{Path, PathBuf};
use std::process::Command;

use greentic_runner_desktop::{RunOptions, RunStatus, run_pack_with_options};
use serde_json::Value;

const REPRO_PACK_PATH: &str = "/tmp/ollama-runtime-bundle/packs/pack.gtpack";
const REPRO_INPUT_PATH: &str = "/tmp/ollama-runtime-bundle/state/runs/messaging/ollama-runtime-repro/main/1776707368/input.json";

#[test]
#[ignore = "requires local Ollama repro bundle artifacts under /tmp"]
fn desktop_api_matches_cli_for_local_ollama_repro() {
    let pack_path = Path::new(REPRO_PACK_PATH);
    let input_path = Path::new(REPRO_INPUT_PATH);
    assert!(
        pack_path.exists(),
        "missing repro pack at {}",
        pack_path.display()
    );
    assert!(
        input_path.exists(),
        "missing repro input at {}",
        input_path.display()
    );

    let input: Value = serde_json::from_slice(
        &std::fs::read(input_path).expect("failed to read local repro input payload"),
    )
    .expect("failed to parse local repro input payload");

    let library = run_pack_with_options(
        pack_path,
        RunOptions {
            entry_flow: Some("main".to_string()),
            input,
            dist_offline: true,
            ..RunOptions::default()
        },
    )
    .expect("desktop API run should complete with a structured failure result");

    assert_eq!(library.status, RunStatus::Failure);
    let library_error = library
        .error
        .as_deref()
        .expect("desktop API run should report an error");
    assert!(
        library_error.contains("OLLAMA_API_KEY"),
        "desktop API should stay on the Ollama config path; got: {library_error}"
    );
    assert!(
        !library_error.contains("provider Openai requires an API key"),
        "desktop API should not fall back to OpenAI defaults; got: {library_error}"
    );

    let cli_binary = workspace_root()
        .join("target")
        .join("debug")
        .join("greentic-runner-cli");
    assert!(
        cli_binary.exists(),
        "missing greentic-runner-cli binary at {}",
        cli_binary.display()
    );

    let cli = Command::new(&cli_binary)
        .arg("--pack")
        .arg(pack_path)
        .arg("--offline")
        .arg("--allow")
        .arg("127.0.0.1,localhost")
        .arg("--input-file")
        .arg(input_path)
        .output()
        .expect("failed to invoke greentic-runner-cli");
    assert!(
        !cli.status.success(),
        "CLI repro should fail so we can compare the failure family"
    );

    let cli_output = format!(
        "{}\n{}",
        String::from_utf8_lossy(&cli.stdout),
        String::from_utf8_lossy(&cli.stderr)
    );
    assert!(
        cli_output.contains("OLLAMA_API_KEY"),
        "CLI should stay on the Ollama config path; got: {cli_output}"
    );
    assert!(
        !cli_output.contains("provider Openai requires an API key"),
        "CLI should not fall back to OpenAI defaults; got: {cli_output}"
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("failed to resolve workspace root")
}
