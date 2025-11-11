use anyhow::Result;

use super::{FetchResponse, HttpResolver, PackResolver};

pub struct GcsResolver {
    inner: HttpResolver,
}

impl GcsResolver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: HttpResolver::new("gcs")?,
        })
    }
}

impl PackResolver for GcsResolver {
    fn scheme(&self) -> &'static str {
        "gcs"
    }

    fn fetch(&self, locator: &str) -> Result<FetchResponse> {
        self.inner.fetch(locator)
    }
}
