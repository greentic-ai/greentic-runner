use anyhow::Result;

use super::{FetchResponse, HttpResolver, PackResolver};

pub struct OciResolver {
    inner: HttpResolver,
}

impl OciResolver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: HttpResolver::new("oci")?,
        })
    }
}

impl PackResolver for OciResolver {
    fn scheme(&self) -> &'static str {
        "oci"
    }

    fn fetch(&self, locator: &str) -> Result<FetchResponse> {
        self.inner.fetch(locator)
    }
}
