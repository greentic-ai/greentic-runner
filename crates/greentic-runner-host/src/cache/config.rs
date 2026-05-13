use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub root: PathBuf,
    pub disk_enabled: bool,
    pub memory_enabled: bool,
    pub disk_max_bytes: Option<u64>,
    pub memory_max_bytes: u64,
    pub lfu_protect_hits: u64,
}

impl CacheConfig {
    pub fn disk_root(&self, engine_profile_id: &str) -> PathBuf {
        self.root
            .join("v1")
            .join(path_safe_cache_segment(engine_profile_id))
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        let root = std::env::var_os("GREENTIC_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".greentic/cache/components"));
        let cache_disabled =
            env_flag_set("GREENTIC_NO_CACHE") || env_flag_set("GREENTIC_DISABLE_CACHE");
        Self {
            root,
            disk_enabled: !cache_disabled,
            memory_enabled: !cache_disabled,
            disk_max_bytes: Some(5 * 1024 * 1024 * 1024),
            memory_max_bytes: 512 * 1024 * 1024,
            lfu_protect_hits: 3,
        }
    }
}

fn env_flag_set(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

pub(crate) fn path_safe_cache_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            {
                '_'
            } else {
                ch
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn contains_windows_invalid_filename_char(value: &str) -> bool {
        value.chars().any(|ch| {
            ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
    }

    #[test]
    fn disk_root_uses_path_safe_engine_profile_segment() {
        let config = CacheConfig {
            root: PathBuf::from("cache-root"),
            ..CacheConfig::default()
        };

        let disk_root = config.disk_root("sha256:abc");
        let profile_segment = disk_root
            .file_name()
            .and_then(|segment| segment.to_str())
            .expect("profile segment");

        assert_eq!(profile_segment, "sha256_abc");
        assert!(!contains_windows_invalid_filename_char(profile_segment));
    }

    #[test]
    fn path_safe_cache_segment_replaces_windows_invalid_chars() {
        let sanitized = path_safe_cache_segment("<>:\"/\\|?*\u{0001}ok");

        assert_eq!(sanitized, "__________ok");
    }
}
