use std::path::Path;

use wit_parser::Resolve;

#[test]
fn staged_wit_packages_are_valid() {
    let staged_root = Path::new(env!("WIT_STAGING_DIR"));
    let entries = std::fs::read_dir(staged_root)
        .unwrap_or_else(|_| panic!("missing staged WIT packages in {}", staged_root.display()));

    for entry in entries {
        let entry = entry.expect("read staged entry");
        if !entry.path().is_dir() {
            continue;
        }
        let mut resolve = Resolve::new();
        resolve
            .push_dir(entry.path())
            .unwrap_or_else(|err| panic!("failed to parse {}: {err}", entry.path().display()));
    }
}
