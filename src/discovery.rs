//! Repository discovery and workspace resolution helpers.

use anyhow::Context;
use std::{
    collections::HashSet,
    env, fs as stdfs,
    path::{Path, PathBuf},
};

use crate::config::EdgeConfig;

pub(crate) fn discover_repositories(roots: &[PathBuf]) -> anyhow::Result<Vec<String>> {
    let mut discovered = Vec::new();
    let mut seen = HashSet::new();

    for root in roots {
        discover_repositories_from_root(root, &mut discovered, &mut seen)?;
    }

    discovered.sort();
    Ok(discovered)
}

fn discover_repositories_from_root(
    directory: &Path,
    discovered: &mut Vec<String>,
    seen: &mut HashSet<String>,
) -> anyhow::Result<()> {
    if contains_git_dir(directory)?
        && let Some(name) = directory.file_name().and_then(|value| value.to_str())
    {
        let trimmed = name.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
            discovered.push(trimmed.to_string());
        }
    }

    for entry in stdfs::read_dir(directory)
        .with_context(|| format!("failed to read repository root {}", directory.display()))?
    {
        let entry = entry.with_context(|| format!("failed to inspect {}", directory.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to read file type for {}", entry.path().display()))?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }

        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if should_skip_repo_scan_directory(name) {
            continue;
        }

        discover_repositories_from_root(&path, discovered, seen)?;
    }

    Ok(())
}

pub(crate) fn resolve_repo_root(config: &EdgeConfig, repo_name: &str) -> anyhow::Result<PathBuf> {
    let mut search_roots = vec![config.workspace_root.clone()];
    for root in &config.allowed_repo_roots {
        if search_roots.iter().any(|existing| existing == root) {
            continue;
        }

        search_roots.push(root.clone());
    }

    for root in search_roots {
        if let Some(repo_root) = find_repo_root_in_directory(&root, repo_name)? {
            return Ok(repo_root);
        }
    }

    anyhow::bail!("workspace repository `{repo_name}` was not found")
}

fn find_repo_root_in_directory(
    directory: &Path,
    repo_name: &str,
) -> anyhow::Result<Option<PathBuf>> {
    if contains_git_dir(directory)?
        && directory
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.trim() == repo_name)
    {
        return Ok(Some(directory.to_path_buf()));
    }

    for entry in stdfs::read_dir(directory)
        .with_context(|| format!("failed to read repository root {}", directory.display()))?
    {
        let entry = entry.with_context(|| format!("failed to inspect {}", directory.display()))?;
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to read file type for {}", entry.path().display()))?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }

        let path = entry.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if should_skip_repo_scan_directory(name) {
            continue;
        }

        if let Some(repo_root) = find_repo_root_in_directory(&path, repo_name)? {
            return Ok(Some(repo_root));
        }
    }

    Ok(None)
}

fn contains_git_dir(directory: &Path) -> anyhow::Result<bool> {
    let git_path = directory.join(".git");
    match stdfs::metadata(&git_path) {
        Ok(metadata) => Ok(metadata.is_dir() || metadata.is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("failed to inspect {}", git_path.display()))
        }
    }
}

fn should_skip_repo_scan_directory(name: &str) -> bool {
    matches!(
        name,
        ".git" | ".elowen" | "node_modules" | "target" | "dist" | "build" | ".next"
    )
}

pub(crate) fn detect_device_id() -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_ascii_lowercase().replace(' ', "-"))
        .unwrap_or_else(|| "elowen-edge".to_string())
}

pub(crate) fn detect_device_name(device_id: &str) -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| device_id.to_string())
}
