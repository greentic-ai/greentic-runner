#![allow(unused)]

use anyhow::Result;

#[tokio::test]
#[ignore = "requires live pack + telegram token"]
async fn weather_smoke() -> Result<()> {
    // TODO: launch host binary and assert webhook flow
    Ok(())
}
