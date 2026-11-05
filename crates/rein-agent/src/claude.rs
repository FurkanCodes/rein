//! Claude Code's own files that rein reads, so what you set up there works
//! in rein without setting it up again: the plugins you turned on, whose
//! skills and hooks rein takes as Claude Code does.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// A plugin you turned on in Claude Code, as it's installed for you.
#[derive(Debug, Clone, PartialEq)]
pub struct Plugin {
    /// Its name, as its manifest gives it, or as you turned it on.
    pub name: String,
    /// Where it's installed: its manifest, skills and hooks are in it.
    pub root: PathBuf,
    /// Its `.claude-plugin/plugin.json`, as it is: `Null` if it has none.
    pub manifest: serde_json::Value,
}

/// The plugins on in Claude Code's settings in `claude` (`~/.claude`),
/// installed for you, in the order of their names. One whose name a
/// skill's or a hook's couldn't be named after is left out.
pub fn plugins(claude: &Path) -> Vec<Plugin> {
    #[derive(Deserialize)]
    struct Settings {
        #[serde(default, rename = "enabledPlugins")]
        enabled: HashMap<String, serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct Installed {
        #[serde(default)]
        plugins: HashMap<String, Vec<Install>>,
    }
    #[derive(Deserialize)]
    struct Install {
        #[serde(default)]
        scope: String,
        #[serde(rename = "installPath")]
        path: PathBuf,
    }
    let read = |path: PathBuf| std::fs::read(path).ok();
    let Some(settings) = read(claude.join("settings.json")).and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok()) else { return Vec::new() };
    let Some(installed) = read(claude.join("plugins/installed_plugins.json")).and_then(|bytes| serde_json::from_slice::<Installed>(&bytes).ok()) else { return Vec::new() };
    let mut on: Vec<&String> = settings.enabled.iter().filter(|(_, value)| **value == serde_json::Value::Bool(true)).map(|(key, _)| key).collect();
    on.sort();
    let mut plugins = Vec::new();
    for key in on {
        let Some(install) = installed.plugins.get(key).and_then(|installs| installs.iter().find(|install| install.scope == "user")) else { continue };
        let Ok(root) = install.path.canonicalize() else { continue };
        let manifest: serde_json::Value = read(root.join(".claude-plugin/plugin.json")).and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        let fallback = key.split('@').next().unwrap_or(key);
        let name = manifest.get("name").and_then(serde_json::Value::as_str).filter(|name| crate::skills::fit_name(name)).unwrap_or(fallback).to_owned();
        if crate::skills::fit_name(&name) {
            plugins.push(Plugin { name, root, manifest });
        }
    }
    plugins
}

/// Whether `path`, as a plugin's manifest names it, stays inside the
/// plugin: not absolute, and never going up.
pub fn inside(path: &str) -> bool {
    let path = Path::new(path);
    !path.is_absolute() && !path.components().any(|part| matches!(part, std::path::Component::ParentDir))
}

/// The paths a manifest's `key` names: one, or a list.
pub fn named(manifest: &serde_json::Value, key: &str) -> Vec<String> {
    match manifest.get(key) {
        Some(serde_json::Value::String(path)) => vec![path.clone()],
        Some(serde_json::Value::Array(paths)) => paths.iter().filter_map(serde_json::Value::as_str).map(str::to_owned).collect(),
        _ => Vec::new(),
    }
}
