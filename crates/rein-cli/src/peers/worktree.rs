//! A workspace's own copy of the repository: a git worktree beside it, on
//! a branch named after the workspace, so the reins of two workspaces
//! never change each other's files.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// The worktree for `workspace` of the repository at `here`, made the
/// first time: `~/projects/shop` and workspace `health` work in
/// `~/projects/shop-health`, on branch `health`.
pub(crate) async fn enter(here: &Path, workspace: &str) -> Result<PathBuf, String> {
    let top = PathBuf::from(git(here, &["rev-parse", "--show-toplevel"]).await.map_err(|e| format!("--worktree works in a git repository: {e}"))?);
    let slug: String = workspace.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '-' }).collect();
    let name = top.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let parent = top.parent().ok_or("the repository has no folder above it")?;
    let tree = parent.join(format!("{name}-{slug}"));
    if tree.join(".git").exists() {
        return Ok(tree);
    }
    if tree.exists() {
        return Err(format!("{} is there already, and isn't a worktree", tree.display()));
    }
    let branch = format!("refs/heads/{workspace}");
    let path = tree.to_string_lossy().into_owned();
    if git(&top, &["rev-parse", "--verify", "--quiet", &branch]).await.is_ok() {
        git(&top, &["worktree", "add", &path, workspace]).await?;
    } else {
        git(&top, &["worktree", "add", "-b", workspace, &path]).await?;
    }
    Ok(tree)
}

/// What `git args` says in `dir`, or why it failed. Its output is small:
/// a path or a branch, never a file's contents.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut command = tokio::process::Command::new("git");
    command.args(args).current_dir(dir).stdin(std::process::Stdio::null()).kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .map_err(|_| format!("git {} took too long", args.join(" ")))?
        .map_err(|e| format!("can't run git: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {}", args.join(" "), said.lines().next().unwrap_or("failed")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_workspace_gets_a_worktree_beside_the_repository_on_its_branch() {
        let dir = std::env::temp_dir().join(format!("rein-worktree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("shop");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [&["init", "-q", "-b", "main"][..], &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "start"]] {
            git(&repo, args).await.unwrap();
        }
        let tree = enter(&repo, "health").await.unwrap();
        assert_eq!(tree.file_name().unwrap(), "shop-health");
        assert_eq!(git(&tree, &["branch", "--show-current"]).await.unwrap(), "health");
        assert_eq!(enter(&repo, "health").await.unwrap(), tree, "the second time, the same one");
        assert_eq!(super::super::default_workspace(&tree), "health");
        assert!(enter(&dir, "x").await.unwrap_err().contains("git repository"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
