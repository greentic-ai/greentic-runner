use std::io::copy;

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use tempfile::NamedTempFile;

use super::{FetchResponse, PackResolver};

pub struct HttpResolver {
    scheme: &'static str,
    client: Client,
}

impl HttpResolver {
    pub fn new(scheme: &'static str) -> Result<Self> {
        Ok(Self {
            scheme,
            client: Client::builder().build()?,
        })
    }
}

impl PackResolver for HttpResolver {
    fn scheme(&self) -> &'static str {
        self.scheme
    }

    fn fetch(&self, locator: &str) -> Result<FetchResponse> {
        let mut response = self
            .client
            .get(locator)
            .send()
            .with_context(|| format!("failed to download {}", locator))?
            .error_for_status()
            .with_context(|| format!("download failed {}", locator))?;

        let mut temp = NamedTempFile::new().context("failed to allocate temp file for download")?;
        {
            let mut writer = temp.as_file_mut();
            copy(&mut response, &mut writer).context("failed to stream HTTP content")?;
        }
        Ok(FetchResponse::from_temp(temp.into_temp_path()))
    }
}
