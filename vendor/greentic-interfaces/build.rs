use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use wit_bindgen_core::wit_parser::Resolve;
use wit_bindgen_core::Files;
use wit_bindgen_rust::Opts;

fn main() -> Result<(), Box<dyn Error>> {
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);
    let staged_root = out_dir.join("wit-staging");
    reset_directory(&staged_root)?;

    let wit_root = Path::new("wit");
    let mut package_paths = Vec::new();
    discover_packages(wit_root, &mut package_paths)?;

    let mut staged = HashSet::new();
    for package_path in package_paths {
        let package_ref = read_package_ref(&package_path)?;
        if staged.insert(package_ref) {
            stage_package(&package_path, &staged_root, wit_root)?;
        }
    }

    let bindings_dir = generate_rust_bindings(&staged_root, &out_dir)?;

    println!("cargo:rustc-env=WIT_STAGING_DIR={}", staged_root.display());
    println!(
        "cargo:rustc-env=GREENTIC_INTERFACES_BINDINGS={}",
        bindings_dir.display()
    );

    Ok(())
}

fn stage_package(
    src_path: &Path,
    staged_root: &Path,
    wit_root: &Path,
) -> Result<(), Box<dyn Error>> {
    let package_ref = read_package_ref(src_path)?;
    let dest_dir = staged_root.join(sanitize(&package_ref));
    fs::create_dir_all(&dest_dir)?;
    fs::copy(src_path, dest_dir.join("package.wit"))?;
    println!("cargo:rerun-if-changed={}", src_path.display());

    let mut visited = HashSet::new();
    stage_dependencies(&dest_dir, src_path, wit_root, &mut visited)?;
    Ok(())
}

fn stage_dependencies(
    parent_dir: &Path,
    source_path: &Path,
    wit_root: &Path,
    visited: &mut HashSet<String>,
) -> Result<(), Box<dyn Error>> {
    let deps = parse_deps(source_path)?;
    if deps.is_empty() {
        return Ok(());
    }

    let deps_dir = parent_dir.join("deps");
    fs::create_dir_all(&deps_dir)?;

    for dep in deps {
        if !visited.insert(dep.clone()) {
            continue;
        }

        let dep_src = wit_path(&dep, wit_root)?;
        let dep_dest = deps_dir.join(sanitize(&dep));
        fs::create_dir_all(&dep_dest)?;
        fs::copy(&dep_src, dep_dest.join("package.wit"))?;
        println!("cargo:rerun-if-changed={}", dep_src.display());

        stage_dependencies(&dep_dest, &dep_src, wit_root, visited)?;
    }

    Ok(())
}

fn wit_path(package_ref: &str, wit_root: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let (pkg, version) = package_ref
        .split_once('@')
        .ok_or_else(|| format!("invalid package reference: {package_ref}"))?;
    let base_pkg = pkg.split('/').next().unwrap_or(pkg);
    let target_root = format!("{base_pkg}@{version}");
    let mut fallback = None;
    if let Some(found) = find_package_recursive(wit_root, package_ref, &target_root, &mut fallback)?
    {
        return Ok(found);
    }
    if let Some(path) = fallback {
        return Ok(path);
    }
    Err(format!("missing WIT source for {package_ref}").into())
}

fn read_package_ref(path: &Path) -> Result<String, Box<dyn Error>> {
    let contents = fs::read_to_string(path)?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("package ") {
            return Ok(rest.trim_end_matches(';').trim().to_string());
        }
    }
    Err(format!("unable to locate package declaration in {}", path.display()).into())
}

fn parse_deps(path: &Path) -> Result<Vec<String>, Box<dyn Error>> {
    let contents = fs::read_to_string(path)?;
    let mut deps = Vec::new();

    for line in contents.lines() {
        let trimmed = line.trim_start();
        let rest = if let Some(rest) = trimmed.strip_prefix("use ") {
            rest
        } else if let Some(rest) = trimmed.strip_prefix("import ") {
            rest
        } else {
            continue;
        };

        let token = rest.split_whitespace().next().unwrap_or("");
        let token = token.trim_end_matches(';');
        let token = token.split(".{").next().unwrap_or(token);
        let token = token.split('{').next().unwrap_or(token);

        let (pkg_part, version_part) = match token.split_once('@') {
            Some(parts) => parts,
            None => continue,
        };

        let pkg = pkg_part;
        let mut version = String::new();
        for ch in version_part.chars() {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
                version.push(ch);
            } else {
                break;
            }
        }
        while version.ends_with('.') {
            version.pop();
        }
        if version.is_empty() {
            continue;
        }

        let dep_ref = format!("{pkg}@{version}");
        if !deps.contains(&dep_ref) {
            deps.push(dep_ref);
        }
    }

    Ok(deps)
}

fn sanitize(package_ref: &str) -> String {
    package_ref.replace([':', '@', '/'], "-")
}

fn generate_rust_bindings(staged_root: &Path, out_dir: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let bindings_dir = out_dir.join("bindings");
    reset_directory(&bindings_dir)?;

    let mut resolve = Resolve::new();
    let staged_pack = staged_root.join("greentic-interfaces-pack-0.1.0");
    let staged_types = staged_root.join("greentic-interfaces-types-0.1.0");

    if !staged_pack.exists() {
        return Err(format!("expected staged WIT package at {}", staged_pack.display()).into());
    }

    let deps_dir = staged_pack.join("deps");
    if staged_types.exists() {
        copy_dir_recursive(
            &staged_types,
            &deps_dir.join("greentic-interfaces-types-0.1.0"),
        )?;
    }

    let (pkg, _) = resolve.push_dir(&staged_pack)?;
    let world = resolve.select_world(&[pkg], Some("greentic:interfaces-pack/component@0.1.0"))?;

    let mut files = Files::default();
    let opts = Opts {
        generate_all: true,
        generate_unused_types: true,
        ..Default::default()
    };
    opts.build().generate(&resolve, world, &mut files)?;

    for (name, contents) in files.iter() {
        let dest = bindings_dir.join(name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, contents)?;
    }

    let component_rs = bindings_dir.join("component.rs");
    let default_bindings = bindings_dir.join("bindings.rs");
    if component_rs.exists() {
        if default_bindings.exists() {
            fs::remove_file(&default_bindings)?;
        }
        fs::rename(&component_rs, &default_bindings)?;
    }

    Ok(bindings_dir)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), Box<dyn Error>> {
    if dst.exists() {
        fs::remove_dir_all(dst)?;
    }
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &dest_path)?;
        } else {
            fs::copy(&path, &dest_path)?;
        }
    }
    Ok(())
}

fn reset_directory(path: &Path) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    fs::create_dir_all(path)?;
    Ok(())
}

fn discover_packages(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let package_file = path.join("package.wit");
            if package_file.exists() {
                out.push(package_file);
            }
            discover_packages(&path, out)?;
        } else if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("wit") {
            out.push(path);
        }
    }
    Ok(())
}

fn find_package_recursive(
    dir: &Path,
    package_ref: &str,
    target_root: &str,
    fallback: &mut Option<PathBuf>,
) -> Result<Option<PathBuf>, Box<dyn Error>> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let package_file = path.join("package.wit");
            if package_file.exists() {
                let entry_package = read_package_ref(&package_file)?;
                if entry_package == package_ref {
                    return Ok(Some(package_file));
                }
                if fallback.is_none() && entry_package == target_root {
                    *fallback = Some(package_file.clone());
                }
            }
            if let Some(found) = find_package_recursive(&path, package_ref, target_root, fallback)?
            {
                return Ok(Some(found));
            }
        } else if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("wit") {
            let entry_package = read_package_ref(&path)?;
            if entry_package == package_ref {
                return Ok(Some(path));
            }
            if fallback.is_none() && entry_package == target_root {
                *fallback = Some(path.clone());
            }
        }
    }
    Ok(None)
}
