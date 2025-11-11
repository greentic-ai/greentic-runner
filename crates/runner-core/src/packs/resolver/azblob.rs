use anyhow::Result;

use super::{FetchResponse, HttpResolver, PackResolver};

pub struct AzBlobResolver {
    inner: HttpResolver,
}

impl AzBlobResolver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: HttpResolver::new("azblob")?,
        })
    }
}

impl PackResolver for AzBlobResolver {
    fn scheme(&self) -> &'static str {
        "azblob"
    }

    fn fetch(&self, locator: &str) -> Result<FetchResponse> {
        self.inner.fetch(locator)
    }
}
