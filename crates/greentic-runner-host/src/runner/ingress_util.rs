use axum::body::Body;
use axum::http::StatusCode;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use lru::LruCache;
use parking_lot::Mutex;
use serde_json::Value;

pub fn mark_processed(cache: &Mutex<LruCache<String, Value>>, key: &str) -> bool {
    let mut cache = cache.lock();
    if cache.get(key).is_some() {
        true
    } else {
        cache.put(key.to_string(), Value::Null);
        false
    }
}

pub async fn collect_body(body: Body) -> Result<Bytes, StatusCode> {
    let mut stream = body.into_data_stream();
    let mut data = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST)?;
        data.extend_from_slice(&chunk);
    }
    Ok(data.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    #[test]
    fn mark_processed_dedupes_keys() {
        let cache = Mutex::new(LruCache::new(NonZeroUsize::new(4).unwrap()));
        assert!(!mark_processed(&cache, "evt-1"));
        assert!(mark_processed(&cache, "evt-1"));
        assert!(!mark_processed(&cache, "evt-2"));
    }

    #[tokio::test]
    async fn collect_body_returns_request_bytes() {
        let body = Body::from(r#"{"hello":"world"}"#);
        let bytes = collect_body(body).await.expect("collect body");
        assert_eq!(bytes, Bytes::from_static(br#"{"hello":"world"}"#));
    }
}
