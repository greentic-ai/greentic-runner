//! Reads the knowledge corpus baked into a pack (W5) and turns it into
//! [`KnowledgeChunk`]s for first-boot ingest (W4 4c). Compiled only with the
//! `knowledge-chronicle` feature.
//!
//! The designer bakes a `knowledge_corpus.json` annotation at the pack root plus
//! the raw document text as `assets/knowledge/*.txt`. The annotation is NOT
//! pre-chunked — it lists the asset files — so this module reads each asset and
//! splits it into overlapping character windows. Chunk ingest downstream is
//! idempotent (Chronicle keys chunks by a deterministic content hash), so
//! re-reading the same corpus on every boot is safe.

use std::sync::Arc;

use greentic_aw_runtime::knowledge::KnowledgeChunk;
use serde::Deserialize;

use crate::pack::PackRuntime;

const CORPUS_SIDECAR: &str = "knowledge_corpus.json";
/// Window size (in characters) for splitting a document into chunks.
const CHUNK_CHARS: usize = 1500;
/// Overlap (in characters) carried between consecutive chunks so a passage that
/// straddles a window boundary still appears intact in at least one chunk.
const CHUNK_OVERLAP_CHARS: usize = 200;

/// The subset of the `knowledge_corpus.json` annotation this reader needs. Other
/// fields (version, strategy, provider ids, top_k, counters) are ignored.
#[derive(Debug, Deserialize)]
struct CorpusAnnotation {
    #[serde(default)]
    files: Vec<CorpusFile>,
}

#[derive(Debug, Deserialize)]
struct CorpusFile {
    /// Archive-relative path of the baked text, e.g. `assets/knowledge/faq.txt`.
    asset_path: String,
    /// Human-facing source name, e.g. `faq.pdf`. Used as the chunk `doc_id`.
    #[serde(default)]
    original_name: String,
}

/// Collect and chunk the knowledge corpus across every pack. Returns an empty
/// vec when no pack carries a corpus (the common case), so callers can skip ingest
/// cheaply. Malformed annotations / missing assets are logged and skipped rather
/// than aborting the whole runtime.
pub fn collect(packs: &[Arc<PackRuntime>]) -> Vec<KnowledgeChunk> {
    let mut chunks = Vec::new();
    for pack in packs {
        let Some(bytes) = pack.read_pack_file(CORPUS_SIDECAR) else {
            continue;
        };
        let pack_id = pack.metadata().pack_id.clone();
        let annotation: CorpusAnnotation = match serde_json::from_slice(&bytes) {
            Ok(annotation) => annotation,
            Err(error) => {
                tracing::warn!(pack_id = %pack_id, error = %error, "knowledge: malformed knowledge_corpus.json; skipping pack corpus");
                continue;
            }
        };
        for file in &annotation.files {
            let Some(raw) = pack.read_pack_file(&file.asset_path) else {
                tracing::warn!(pack_id = %pack_id, asset = %file.asset_path, "knowledge: corpus asset missing from pack; skipping");
                continue;
            };
            let text = match String::from_utf8(raw) {
                Ok(text) => text,
                Err(_) => {
                    tracing::warn!(pack_id = %pack_id, asset = %file.asset_path, "knowledge: corpus asset is not UTF-8; skipping");
                    continue;
                }
            };
            let doc_id = if file.original_name.is_empty() {
                file.asset_path.clone()
            } else {
                file.original_name.clone()
            };
            append_chunks(&mut chunks, &pack_id, &file.asset_path, &doc_id, &text);
        }
    }
    chunks
}

/// Split `text` into overlapping character windows and push one [`KnowledgeChunk`]
/// per window. Whitespace-only documents produce no chunks.
fn append_chunks(
    out: &mut Vec<KnowledgeChunk>,
    pack_id: &str,
    asset_path: &str,
    doc_id: &str,
    text: &str,
) {
    if text.trim().is_empty() {
        return;
    }
    // Character-based windowing keeps multi-byte UTF-8 boundaries valid (slicing
    // by byte offset could split a codepoint).
    let chars: Vec<char> = text.chars().collect();
    let step = CHUNK_CHARS.saturating_sub(CHUNK_OVERLAP_CHARS).max(1);
    let mut start = 0;
    let mut chunk_index = 0;
    while start < chars.len() {
        let end = (start + CHUNK_CHARS).min(chars.len());
        let window: String = chars[start..end].iter().collect();
        if !window.trim().is_empty() {
            let mut metadata = serde_json::Map::new();
            metadata.insert("pack_id".into(), pack_id.into());
            metadata.insert("asset_path".into(), asset_path.into());
            out.push(KnowledgeChunk {
                doc_id: doc_id.to_string(),
                chunk_index,
                text: window,
                metadata,
                // Pack-baked text carries no pre-computed vector; the backend
                // embeds it at ingest time.
                embedding: None,
            });
            chunk_index += 1;
        }
        if end == chars.len() {
            break;
        }
        start += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_a_single_chunk() {
        let mut out = Vec::new();
        append_chunks(
            &mut out,
            "p",
            "assets/knowledge/a.txt",
            "a.txt",
            "hello world",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].doc_id, "a.txt");
        assert_eq!(out[0].chunk_index, 0);
        assert_eq!(out[0].text, "hello world");
        assert_eq!(
            out[0].metadata.get("asset_path").and_then(|v| v.as_str()),
            Some("assets/knowledge/a.txt")
        );
    }

    #[test]
    fn long_text_windows_with_overlap_and_sequential_indices() {
        let text = "x".repeat(CHUNK_CHARS * 2 + 100);
        let mut out = Vec::new();
        append_chunks(&mut out, "p", "a", "doc", &text);
        assert!(
            out.len() >= 3,
            "expected multiple windows, got {}",
            out.len()
        );
        for (i, c) in out.iter().enumerate() {
            assert_eq!(c.chunk_index, i, "chunk indices must be sequential");
            assert!(c.text.chars().count() <= CHUNK_CHARS);
        }
    }

    #[test]
    fn whitespace_only_text_produces_no_chunks() {
        let mut out = Vec::new();
        append_chunks(&mut out, "p", "a", "doc", "   \n\t  ");
        assert!(out.is_empty());
    }

    #[test]
    fn multibyte_text_does_not_panic_and_preserves_codepoints() {
        // Emoji + accented chars across a window boundary must stay valid UTF-8.
        let text = "café☕".repeat(CHUNK_CHARS);
        let mut out = Vec::new();
        append_chunks(&mut out, "p", "a", "doc", &text);
        assert!(!out.is_empty());
        let rejoined: String = out.iter().map(|c| c.text.as_str()).collect();
        assert!(rejoined.contains("café☕"));
    }
}
