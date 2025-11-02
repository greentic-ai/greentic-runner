use crate::newrunner::error::GResult;
use crate::newrunner::registry::{Adapter, AdapterCall};
use async_trait::async_trait;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

#[async_trait]
pub trait AdapterBridge: Send + Sync {
    async fn invoke(&self, call: AdapterCall) -> GResult<Value>;
}

#[async_trait]
impl<T> Adapter for T
where
    T: AdapterBridge,
{
    async fn call(&self, call: &AdapterCall) -> GResult<Value> {
        self.invoke(call.clone()).await
    }
}

pub struct FnAdapterBridge {
    inner: Arc<
        dyn Fn(AdapterCall) -> Pin<Box<dyn Future<Output = GResult<Value>> + Send>> + Send + Sync,
    >,
}

impl FnAdapterBridge {
    pub fn new<F, Fut>(func: F) -> Self
    where
        F: Send + Sync + 'static + Fn(AdapterCall) -> Fut,
        Fut: Future<Output = GResult<Value>> + Send + 'static,
    {
        Self {
            inner: Arc::new(move |call: AdapterCall| {
                let fut = func(call);
                Box::pin(fut)
            }),
        }
    }
}

#[async_trait]
impl AdapterBridge for FnAdapterBridge {
    async fn invoke(&self, call: AdapterCall) -> GResult<Value> {
        (self.inner)(call).await
    }
}
