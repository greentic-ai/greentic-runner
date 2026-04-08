use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use greentic_types::cbor::canonical;

// Locale -> (key -> translated message)
pub type LocaleBundle = BTreeMap<String, BTreeMap<String, String>>;

// Reads `assets/i18n/*.json` locale maps and returns stable BTreeMap ordering.
// Extend here if you need stricter file validation rules.
pub fn load_locale_files(dir: &Path) -> Result<LocaleBundle, String> {
    let mut locales = LocaleBundle::new();
    if !dir.exists() {
        return Ok(locales);
    }
    for entry in fs::read_dir(dir).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        // locales.json is metadata, not a translation dictionary.
        if stem == "locales" {
            continue;
        }
        let raw = fs::read_to_string(&path).map_err(|err| err.to_string())?;
        let map: BTreeMap<String, String> =
            serde_json::from_str(&raw).map_err(|err| err.to_string())?;
        locales.insert(stem.to_string(), map);
    }
    Ok(locales)
}

pub fn pack_locales_to_cbor(locales: &LocaleBundle) -> Result<Vec<u8>, String> {
    canonical::to_canonical_cbor_allow_floats(locales).map_err(|err| err.to_string())
}

#[allow(dead_code)]
// Runtime decode helper used by src/i18n.rs.
pub fn unpack_locales_from_cbor(bytes: &[u8]) -> Result<LocaleBundle, String> {
    canonical::from_cbor(bytes).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip_contains_en() {
        let mut locales = LocaleBundle::new();
        let mut en = BTreeMap::new();
        en.insert("qa.install.title".to_string(), "Install".to_string());
        locales.insert("en".to_string(), en);

        let cbor = pack_locales_to_cbor(&locales).expect("pack locales");
        let decoded = unpack_locales_from_cbor(&cbor).expect("decode locales");
        assert!(decoded.contains_key("en"));
    }
}
