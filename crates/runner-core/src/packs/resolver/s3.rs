use anyhow::Result;

use super::{FetchResponse, HttpResolver, PackResolver};

pub struct S3Resolver {
    inner: HttpResolver,
}

impl S3Resolver {
    pub fn new() -> Result<Self> {
        Ok(Self {
            inner: HttpResolver::new("s3")?,
        })
    }
}

impl PackResolver for S3Resolver {
    fn scheme(&self) -> &'static str {
        "s3"
    }

    fn fetch(&self, locator: &str) -> Result<FetchResponse> {
        self.inner.fetch(locator)
    }
}
