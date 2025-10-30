#![allow(unused)]

use anyhow::Result;

#[tokio::test]
#[ignore = "timer/webhook adapters tracked in M2"]
async fn webhook_timer() -> Result<()> {
    // TODO: implement integration coverage once adapters land
    Ok(())
}
