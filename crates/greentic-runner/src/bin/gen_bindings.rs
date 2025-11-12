#![allow(clippy::missing_errors_doc)]

use anyhow::{Context, Result, bail};
use clap::Parser;
use greentic_runner::gen_bindings::{self, GeneratorOptions, component};
use std::{fs, path::PathBuf};

#[derive(Debug, Parser)]
#[command(
    name = "greentic-gen-bindings",
    about = "Generate bindings.yaml hints from a pack directory or component"
)]
struct Cli {
    /// Pack directory that exposes pack.yaml + flow annotations
    #[arg(long, value_name = "DIR")]
    pack: Option<PathBuf>,

    /// Compiled pack component (.wasm) to inspect
    #[arg(long, value_name = "FILE")]
    component: Option<PathBuf>,

    /// Output path for the generated bindings
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,

    /// Try to complete missing hints with safe defaults
    #[arg(long)]
    complete: bool,

    /// Fail if information is missing instead of inferring
    #[arg(long)]
    strict: bool,

    /// Pretty-print the emitted YAML
    #[arg(long)]
    pretty: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.pack.is_none() && cli.component.is_none() {
        bail!("either --pack or --component is required");
    }

    let component_features = if let Some(component_path) = cli.component.clone() {
        let features = component::analyze_component(&component_path)?;
        println!("component features: {:?}", features);
        Some(features)
    } else {
        None
    };

    let common_opts = GeneratorOptions {
        strict: cli.strict,
        complete: cli.complete,
        component: component_features.clone(),
    };

    if let Some(pack_dir) = cli.pack {
        let metadata = gen_bindings::load_pack(&pack_dir)?;
        let bindings = gen_bindings::generate_bindings(&metadata, common_opts)?;
        let serialized = serde_yaml::to_string(&bindings)?;
        let out_path = cli
            .out
            .unwrap_or_else(|| pack_dir.join("bindings.generated.yaml"));
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        fs::write(&out_path, serialized)
            .with_context(|| format!("failed to write {}", out_path.display()))?;
        println!("generated bindings → {}", out_path.display());
    } else if let Some(component_path) = cli.component {
        if component_features.is_some() {
            println!("component-only analysis complete");
            return Ok(());
        }
        bail!(
            "component inspection is not supported yet (tried: {})",
            component_path.display()
        );
    }

    Ok(())
}
