use std::path::PathBuf;

use anyhow::Result;
use greentic_runner::desktop::{DevProfile, Profile, Runner};
use greentic_runner::runner::mocks::MocksConfig;

fn main() -> Result<()> {
    let pack_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures/demo.gtpack");

    let runner = Runner::new()
        .profile(Profile::Dev(DevProfile::default()))
        .with_mocks(MocksConfig::default());

    let result = runner.run_pack(pack_path)?;

    println!(
        "Pack run completed with status {:?}\nSession: {}\nArtifacts: {}",
        result.status,
        result.session_id,
        result.artifacts_dir.display()
    );

    Ok(())
}
