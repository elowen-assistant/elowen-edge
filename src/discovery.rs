//! Repository discovery and workspace resolution helpers.

use anyhow::Context;
use std::{
    collections::{BTreeSet, HashSet},
    env, fs as stdfs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::config::EdgeConfig;
use crate::contracts::DeviceRepository;

pub(crate) fn discover_repository_catalog(
    roots: &[PathBuf],
    excluded_paths: &[PathBuf],
) -> anyhow::Result<Vec<DeviceRepository>> {
    let mut discovered = Vec::new();
    let mut seen = HashSet::new();

    for root in roots {
        discover_repositories_from_root(root, excluded_paths, &mut discovered, &mut seen)?;
    }

    discovered.sort();
    Ok(discovered)
}

pub(crate) fn discover_repositories(
    roots: &[PathBuf],
    excluded_paths: &[PathBuf],
) -> anyhow::Result<Vec<String>> {
    Ok(discover_repository_catalog(roots, excluded_paths)?
        .into_iter()
        .map(|repository| repository.name)
        .collect())
}

fn discover_repositories_from_root(
    directory: &Path,
    excluded_paths: &[PathBuf],
    discovered: &mut Vec<DeviceRepository>,
    seen: &mut HashSet<String>,
) -> anyhow::Result<()> {
    if excluded_paths.iter().any(|excluded| directory.starts_with(excluded)) {
        return Ok(());
    }

    if contains_git_dir(directory)?
        && let Some(name) = directory.file_name().and_then(|value| value.to_str())
    {
        let trimmed = name.trim();
        if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
            discovered.push(DeviceRepository {
                name: trimmed.to_string(),
                branches: list_repository_branches(directory)?,
            });
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

        discover_repositories_from_root(&path, excluded_paths, discovered, seen)?;
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
        if let Some(repo_root) =
            find_repo_root_in_directory(&root, &config.excluded_repo_paths, repo_name)?
        {
            return Ok(repo_root);
        }
    }

    anyhow::bail!("workspace repository `{repo_name}` was not found")
}

fn find_repo_root_in_directory(
    directory: &Path,
    excluded_paths: &[PathBuf],
    repo_name: &str,
) -> anyhow::Result<Option<PathBuf>> {
    if excluded_paths.iter().any(|excluded| directory.starts_with(excluded)) {
        return Ok(None);
    }

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

        if let Some(repo_root) = find_repo_root_in_directory(&path, excluded_paths, repo_name)? {
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

fn list_repository_branches(repo_root: &Path) -> anyhow::Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["for-each-ref", "--format=%(refname:short)", "refs/heads"])
        .output()
        .with_context(|| format!("failed to list branches for {}", repo_root.display()))?;

    if !output.status.success() {
        anyhow::bail!(
            "git branch discovery failed for {}: {}",
            repo_root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut branches = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| std::str::from_utf8(line).ok())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();

    branches.sort_by_key(|branch| branch_priority(branch));
    Ok(branches)
}

fn branch_priority(branch: &str) -> (u8, String) {
    let priority = match branch {
        "main" => 0,
        "master" => 1,
        _ => 2,
    };
    (priority, branch.to_string())
}

#[cfg(test)]
mod tests {
    use super::{discover_repositories, discover_repository_catalog, find_repo_root_in_directory};
    use std::{fs, path::{Path, PathBuf}, process::Command};

    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("elowen-edge-discovery-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn init_git_repo(path: &Path) {
        fs::create_dir_all(path).unwrap();
        let init = Command::new("git")
            .arg("init")
            .arg("--initial-branch=main")
            .arg(path)
            .output()
            .unwrap();
        assert!(init.status.success(), "{:?}", init);
    }

    #[test]
    fn excluded_paths_are_skipped_during_discovery() {
        let root = unique_temp_dir("scan");
        let visible = root.join("visible-repo");
        let hidden_parent = root.join("private");
        let hidden = hidden_parent.join("hidden-repo");
        init_git_repo(&visible);
        init_git_repo(&hidden);

        let discovered = discover_repositories(
            &[fs::canonicalize(&root).unwrap()],
            &[fs::canonicalize(hidden_parent).unwrap()],
        )
        .unwrap();

        assert_eq!(discovered, vec!["visible-repo".to_string()]);

        let catalog = discover_repository_catalog(
            &[fs::canonicalize(&root).unwrap()],
            &[fs::canonicalize(hidden_parent).unwrap()],
        )
        .unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].name, "visible-repo");

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn excluded_paths_are_not_considered_for_repo_resolution() {
        let root = unique_temp_dir("resolve");
        let hidden_parent = root.join("private");
        let hidden = hidden_parent.join("hidden-repo");
        init_git_repo(&hidden);

        let resolved = find_repo_root_in_directory(
            &fs::canonicalize(&root).unwrap(),
            &[fs::canonicalize(hidden_parent).unwrap()],
            "hidden-repo",
        )
        .unwrap();

        assert!(resolved.is_none());

        let _ = fs::remove_dir_all(root);
    }
}
