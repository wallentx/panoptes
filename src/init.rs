//! User-level provider registration and code-navigation guidance. Machine-specific
//! executable paths and provider integration belong only in user configuration,
//! never in indexed repositories.

use anyhow::{Context, Result, bail};
use dialoguer::{MultiSelect, console::Term, theme::SimpleTheme};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

const SKILL: &str = include_str!("../assets/skills/panoptes/SKILL.md");
const OPENAI_SKILL_YAML: &str = include_str!("../assets/skills/panoptes/agents/openai.yaml");
const MANAGED_SKILL_MARKER: &str = "<!-- panoptes:managed-skill -->";
const GUIDANCE_START: &str = "<!-- panoptes:managed:start -->";
const GUIDANCE_END: &str = "<!-- panoptes:managed:end -->";
const GUIDANCE: &str = r#"## Panoptes code navigation

For coding tasks involving indexed source, prefer the Panoptes MCP tools before
built-in grep, search, or whole-file reads. Use `find` for ranked code context,
`grep` for exhaustive occurrences, `callers` for dependencies and blast radius,
`skeleton` for a file API, and `map` for repository orientation. Work from the
returned paths, spans, and bounded source before reading more. When Panoptes was
used, report its MCP session token-savings total as an estimate versus reading
matched files whole, never as model billing."#;

struct Provider {
    id: &'static str,
    name: &'static str,
    config: &'static str,
    detection: &'static str,
    format: &'static str,
    top_key: &'static str,
}

const PROVIDERS: &[Provider] = &[
    Provider {
        id: "claude",
        name: "Claude Code",
        config: ".claude.json",
        detection: ".claude",
        format: "json",
        top_key: "mcpServers",
    },
    Provider {
        id: "codex",
        name: "Codex",
        config: ".codex/config.toml",
        detection: ".codex",
        format: "toml",
        top_key: "",
    },
    Provider {
        id: "cursor",
        name: "Cursor",
        config: ".cursor/mcp.json",
        detection: ".cursor",
        format: "json",
        top_key: "mcpServers",
    },
    Provider {
        id: "gemini",
        name: "Gemini CLI",
        config: ".gemini/settings.json",
        detection: ".gemini/settings.json",
        format: "json",
        top_key: "mcpServers",
    },
    Provider {
        id: "antigravity",
        name: "Antigravity",
        config: ".gemini/config/mcp_config.json",
        detection: ".gemini/config",
        format: "json",
        top_key: "mcpServers",
    },
    Provider {
        id: "opencode",
        name: "OpenCode",
        config: ".config/opencode/opencode.json",
        detection: ".config/opencode",
        format: "opencode-json",
        top_key: "mcp",
    },
    Provider {
        id: "copilot",
        name: "GitHub Copilot CLI",
        config: ".copilot/mcp-config.json",
        detection: ".copilot",
        format: "json",
        top_key: "mcpServers",
    },
];

#[derive(Debug, Serialize)]
pub struct PlannedWrite {
    pub provider: String,
    pub path: String,
    pub format: &'static str,
    pub action: &'static str,
}

#[derive(Clone, Copy)]
struct Registration {
    present: bool,
    current: bool,
}

/// Show the interactive checkbox picker used by plain `panoptes init`.
pub fn select_providers() -> Result<Vec<String>> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let home = PathBuf::from(home);
    let executable = std::env::current_exe()?.canonicalize()?;
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "panoptes init needs a terminal for provider selection; pass one or more --provider <id> values in scripts"
        );
    }

    let states: Vec<Registration> = PROVIDERS
        .iter()
        .map(|provider| registration(&home, provider, &executable))
        .collect::<Result<_>>()?;
    let items: Vec<String> = PROVIDERS
        .iter()
        .zip(&states)
        .map(|(provider, state)| {
            let detected = home.join(provider.detection).exists();
            format!(
                "{:<20}  ~/{:<34}{}",
                provider.name,
                provider.config,
                if state.present {
                    " registered"
                } else if detected {
                    " detected"
                } else {
                    ""
                }
            )
        })
        .collect();
    let defaults: Vec<bool> = states.iter().map(|state| state.present).collect();
    let selections = MultiSelect::with_theme(&SimpleTheme)
        .with_prompt("Select providers (space toggles, enter confirms)")
        .items(&items)
        .defaults(&defaults)
        .report(false)
        .interact_on(&Term::stderr())
        .context("provider selection failed")?;
    Ok(selections
        .into_iter()
        .map(|index| PROVIDERS[index].id.to_string())
        .collect())
}

/// Reconcile every provider with an interactive checkbox selection.
pub fn reconcile(providers: &[String], dry_run: bool) -> Result<Vec<PlannedWrite>> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let home = PathBuf::from(home);
    let executable = std::env::current_exe()?.canonicalize()?;
    reconcile_at(&home, &executable, providers, dry_run)
}

/// Add or refresh explicitly named registrations without touching other providers.
pub fn register(providers: &[String], dry_run: bool) -> Result<Vec<PlannedWrite>> {
    change_named(providers, dry_run, true)
}

/// Remove explicitly named registrations without touching other providers.
pub fn deregister(providers: &[String], dry_run: bool) -> Result<Vec<PlannedWrite>> {
    change_named(providers, dry_run, false)
}

fn reconcile_at(
    home: &Path,
    executable: &Path,
    selected: &[String],
    dry_run: bool,
) -> Result<Vec<PlannedWrite>> {
    let selected: HashSet<&str> = selected.iter().map(String::as_str).collect();
    for id in &selected {
        provider(id)?;
    }
    let active: HashSet<String> = selected.iter().map(|id| (*id).to_string()).collect();
    if !dry_run {
        sync_guidance(home, &active, true, &mut Vec::new())?;
    }
    let mut writes = Vec::new();
    for provider in PROVIDERS {
        let state = registration(home, provider, executable)?;
        let checked = selected.contains(provider.id);
        let action = match (checked, state.present, state.current) {
            (true, false, _) => Some("register"),
            (true, true, false) => Some("update"),
            (false, true, _) => Some("deregister"),
            _ => None,
        };
        if let Some(action) = action {
            change_provider(home, executable, provider, action, dry_run, &mut writes)?;
        }
    }
    sync_guidance(home, &active, dry_run, &mut writes)?;
    Ok(writes)
}

fn change_named(providers: &[String], dry_run: bool, add: bool) -> Result<Vec<PlannedWrite>> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    let home = PathBuf::from(home);
    let executable = std::env::current_exe()?.canonicalize()?;
    let mut active = HashSet::new();
    for candidate in PROVIDERS {
        if registration(&home, candidate, &executable)?.present {
            active.insert(candidate.id.to_string());
        }
    }
    let mut seen = HashSet::new();
    for id in providers {
        if !seen.insert(id.as_str()) {
            continue;
        }
        let provider = provider(id)?;
        if add {
            active.insert(provider.id.to_string());
        } else {
            active.remove(provider.id);
        }
    }
    if !dry_run {
        sync_guidance(&home, &active, true, &mut Vec::new())?;
    }

    let mut writes = Vec::new();
    seen.clear();
    for id in providers {
        if !seen.insert(id.as_str()) {
            continue;
        }
        let provider = provider(id)?;
        let state = registration(&home, provider, &executable)?;
        let action = match (add, state.present, state.current) {
            (true, _, true) | (false, false, _) => None,
            (true, true, false) => Some("update"),
            (true, false, _) => Some("register"),
            (false, true, _) => Some("deregister"),
        };
        if let Some(action) = action {
            change_provider(&home, &executable, provider, action, dry_run, &mut writes)?;
        }
    }
    sync_guidance(&home, &active, dry_run, &mut writes)?;
    Ok(writes)
}

fn change_provider(
    home: &Path,
    executable: &Path,
    provider: &Provider,
    action: &'static str,
    dry_run: bool,
    writes: &mut Vec<PlannedWrite>,
) -> Result<()> {
    let path = home.join(provider.config);
    writes.push(PlannedWrite {
        provider: provider.id.to_string(),
        path: path.to_string_lossy().into_owned(),
        format: provider.format,
        action,
    });
    if dry_run {
        return Ok(());
    }
    if action == "deregister" {
        match provider.format {
            "toml" => remove_codex(&path)?,
            "json" | "opencode-json" => remove_json(&path, provider.top_key)?,
            _ => unreachable!(),
        }
    } else {
        match provider.format {
            "toml" => upsert_codex(&path, executable)?,
            "opencode-json" => merge_json(
                &path,
                provider.top_key,
                json!({"type":"local", "command":[executable, "mcp"], "enabled":true}),
            )?,
            "json" => merge_json(
                &path,
                provider.top_key,
                json!({"command":executable, "args":["mcp"]}),
            )?,
            _ => unreachable!(),
        }
    }
    Ok(())
}

fn sync_guidance(
    home: &Path,
    active: &HashSet<String>,
    dry_run: bool,
    writes: &mut Vec<PlannedWrite>,
) -> Result<()> {
    let has = |id: &str| active.contains(id);
    let shared = ["codex", "cursor", "gemini", "opencode", "copilot"]
        .into_iter()
        .any(has);
    sync_skill(
        home,
        ".agents/skills/panoptes",
        "shared",
        shared,
        dry_run,
        writes,
    )?;
    sync_skill(
        home,
        ".claude/skills/panoptes",
        "claude",
        has("claude"),
        dry_run,
        writes,
    )?;
    sync_skill(
        home,
        ".gemini/antigravity-cli/skills/panoptes",
        "antigravity",
        has("antigravity"),
        dry_run,
        writes,
    )?;

    let codex = has("codex");
    let codex_agents = home.join(".codex/AGENTS.md");
    let codex_override = home.join(".codex/AGENTS.override.md");
    let override_active =
        codex_override.exists() && !std::fs::read_to_string(&codex_override)?.trim().is_empty();
    sync_marked_guidance(
        &codex_agents,
        "codex",
        codex && !override_active,
        dry_run,
        writes,
    )?;
    sync_marked_guidance(
        &codex_override,
        "codex",
        codex && override_active,
        dry_run,
        writes,
    )?;

    sync_owned_guidance(
        &home.join(".claude/rules/panoptes.md"),
        "claude",
        has("claude"),
        dry_run,
        writes,
    )?;
    sync_marked_guidance(
        &home.join(".gemini/GEMINI.md"),
        "gemini/antigravity",
        has("gemini") || has("antigravity"),
        dry_run,
        writes,
    )?;
    Ok(())
}

fn sync_skill(
    home: &Path,
    relative: &str,
    provider: &str,
    enabled: bool,
    dry_run: bool,
    writes: &mut Vec<PlannedWrite>,
) -> Result<()> {
    let directory = home.join(relative);
    let skill_path = directory.join("SKILL.md");
    let metadata_path = directory.join("agents/openai.yaml");
    let existing = if skill_path.exists() {
        Some(std::fs::read_to_string(&skill_path)?)
    } else {
        None
    };
    if enabled
        && let Some(text) = &existing
        && text != SKILL
        && !text.contains(MANAGED_SKILL_MARKER)
    {
        bail!(
            "refusing to overwrite unmanaged Panoptes skill at {}",
            skill_path.display()
        );
    }

    if enabled {
        let metadata_current =
            metadata_path.exists() && std::fs::read_to_string(&metadata_path)? == OPENAI_SKILL_YAML;
        if existing.as_deref() == Some(SKILL) && metadata_current {
            return Ok(());
        }
        writes.push(PlannedWrite {
            provider: provider.to_string(),
            path: skill_path.to_string_lossy().into_owned(),
            format: "skill",
            action: if existing.is_some() {
                "update"
            } else {
                "install"
            },
        });
        if !dry_run {
            write_private_atomic(&skill_path, SKILL.as_bytes())?;
            write_private_atomic(&metadata_path, OPENAI_SKILL_YAML.as_bytes())?;
        }
    } else if existing
        .as_deref()
        .is_some_and(|text| text.contains(MANAGED_SKILL_MARKER))
    {
        writes.push(PlannedWrite {
            provider: provider.to_string(),
            path: skill_path.to_string_lossy().into_owned(),
            format: "skill",
            action: "remove",
        });
        if !dry_run {
            std::fs::remove_file(&skill_path)?;
            if metadata_path.exists() {
                std::fs::remove_file(&metadata_path)?;
            }
        }
    }
    Ok(())
}

fn managed_guidance() -> String {
    format!("{GUIDANCE_START}\n{GUIDANCE}\n{GUIDANCE_END}\n")
}

fn marked_range(text: &str) -> Result<Option<(usize, usize)>> {
    let start = text.find(GUIDANCE_START);
    let end = text.find(GUIDANCE_END);
    match (start, end) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) if end >= start => {
            let mut end = end + GUIDANCE_END.len();
            if text.as_bytes().get(end) == Some(&b'\n') {
                end += 1;
            }
            Ok(Some((start, end)))
        }
        _ => bail!("Panoptes guidance has an unmatched managed marker"),
    }
}

fn sync_marked_guidance(
    path: &Path,
    provider: &str,
    enabled: bool,
    dry_run: bool,
    writes: &mut Vec<PlannedWrite>,
) -> Result<()> {
    let mut text = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let range = marked_range(&text)
        .with_context(|| format!("invalid Panoptes guidance in {}", path.display()))?;
    let original = text.clone();
    if enabled {
        let block = managed_guidance();
        if let Some((start, end)) = range {
            text.replace_range(start..end, &block);
        } else {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            if !text.trim().is_empty() {
                text.push('\n');
            }
            text.push_str(&block);
        }
    } else if let Some((start, end)) = range {
        text.replace_range(start..end, "");
    }
    if text == original {
        return Ok(());
    }
    writes.push(PlannedWrite {
        provider: provider.to_string(),
        path: path.to_string_lossy().into_owned(),
        format: "markdown",
        action: if enabled {
            if original.is_empty() {
                "install"
            } else {
                "update"
            }
        } else {
            "remove"
        },
    });
    if !dry_run {
        write_private_atomic(path, text.as_bytes())?;
    }
    Ok(())
}

fn sync_owned_guidance(
    path: &Path,
    provider: &str,
    enabled: bool,
    dry_run: bool,
    writes: &mut Vec<PlannedWrite>,
) -> Result<()> {
    let desired = managed_guidance();
    let existing = if path.exists() {
        Some(std::fs::read_to_string(path)?)
    } else {
        None
    };
    if enabled
        && let Some(text) = &existing
        && text != &desired
        && !text.contains(GUIDANCE_START)
    {
        bail!(
            "refusing to overwrite unmanaged Panoptes guidance at {}",
            path.display()
        );
    }
    if enabled && existing.as_deref() != Some(&desired) {
        writes.push(PlannedWrite {
            provider: provider.to_string(),
            path: path.to_string_lossy().into_owned(),
            format: "markdown",
            action: if existing.is_some() {
                "update"
            } else {
                "install"
            },
        });
        if !dry_run {
            write_private_atomic(path, desired.as_bytes())?;
        }
    } else if !enabled
        && existing
            .as_deref()
            .is_some_and(|text| text.contains(GUIDANCE_START))
    {
        writes.push(PlannedWrite {
            provider: provider.to_string(),
            path: path.to_string_lossy().into_owned(),
            format: "markdown",
            action: "remove",
        });
        if !dry_run {
            std::fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn provider(id: &str) -> Result<&'static Provider> {
    PROVIDERS
        .iter()
        .find(|provider| provider.id == id)
        .with_context(|| {
            format!(
                "unsupported provider {id:?}; expected {}",
                PROVIDERS
                    .iter()
                    .map(|provider| provider.id)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn registration(home: &Path, provider: &Provider, executable: &Path) -> Result<Registration> {
    let path = home.join(provider.config);
    if !path.exists() {
        return Ok(Registration {
            present: false,
            current: false,
        });
    }
    if provider.format == "toml" {
        let text = std::fs::read_to_string(&path)?;
        let Some((start, end)) = codex_section_range(&text) else {
            return Ok(Registration {
                present: false,
                current: false,
            });
        };
        let section = &text[start..end];
        let command = format!(
            "command = {}",
            serde_json::to_string(&executable.to_string_lossy())?
        );
        return Ok(Registration {
            present: true,
            current: section.lines().any(|line| line.trim() == command)
                && section
                    .lines()
                    .any(|line| line.trim() == "args = [\"mcp\"]"),
        });
    }

    let root = serde_json::from_str::<Value>(&std::fs::read_to_string(&path)?)
        .with_context(|| format!("refusing to read invalid JSON in {}", path.display()))?;
    let Some(bucket) = root.get(provider.top_key) else {
        return Ok(Registration {
            present: false,
            current: false,
        });
    };
    let bucket = bucket.as_object().with_context(|| {
        format!(
            "{} in {} must be an object",
            provider.top_key,
            path.display()
        )
    })?;
    let Some(entry) = bucket.get("panoptes") else {
        return Ok(Registration {
            present: false,
            current: false,
        });
    };
    let desired = if provider.format == "opencode-json" {
        json!({"type":"local", "command":[executable, "mcp"], "enabled":true})
    } else {
        json!({"command":executable, "args":["mcp"]})
    };
    Ok(Registration {
        present: true,
        current: entry == &desired,
    })
}

fn merge_json(path: &Path, top_key: &str, entry: Value) -> Result<()> {
    let mut root = if path.exists() {
        serde_json::from_str::<Value>(&std::fs::read_to_string(path)?)
            .with_context(|| format!("refusing to rewrite invalid JSON in {}", path.display()))?
    } else {
        json!({})
    };
    let object = root
        .as_object_mut()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let bucket = object.entry(top_key).or_insert_with(|| json!({}));
    let bucket = bucket
        .as_object_mut()
        .with_context(|| format!("{top_key} in {} must be an object", path.display()))?;
    bucket.insert("panoptes".to_string(), entry);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private_atomic(
        path,
        format!("{}\n", serde_json::to_string_pretty(&root)?).as_bytes(),
    )?;
    Ok(())
}

fn remove_json(path: &Path, top_key: &str) -> Result<()> {
    let mut root = serde_json::from_str::<Value>(&std::fs::read_to_string(path)?)
        .with_context(|| format!("refusing to rewrite invalid JSON in {}", path.display()))?;
    let object = root
        .as_object_mut()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let bucket = object
        .get_mut(top_key)
        .with_context(|| format!("{top_key} is missing from {}", path.display()))?
        .as_object_mut()
        .with_context(|| format!("{top_key} in {} must be an object", path.display()))?;
    bucket.remove("panoptes");
    write_private_atomic(
        path,
        format!("{}\n", serde_json::to_string_pretty(&root)?).as_bytes(),
    )?;
    Ok(())
}

fn codex_section_range(text: &str) -> Option<(usize, usize)> {
    let mut start = None;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.split('#').next().unwrap_or(line).trim();
        if start.is_none() {
            if trimmed == "[mcp_servers.panoptes]" {
                start = Some(offset);
            }
        } else if trimmed.starts_with('[') && trimmed.ends_with(']') {
            return Some((start?, offset));
        }
        offset += line.len();
    }
    start.map(|start| (start, text.len()))
}

fn upsert_codex(path: &Path, executable: &Path) -> Result<()> {
    let mut text = if path.exists() {
        std::fs::read_to_string(path)?
    } else {
        String::new()
    };
    let section = format!(
        "[mcp_servers.panoptes]\ncommand = {}\nargs = [\"mcp\"]\n",
        serde_json::to_string(&executable.to_string_lossy())?
    );
    if let Some((start, end)) = codex_section_range(&text) {
        text.replace_range(start..end, &section);
        write_private_atomic(path, text.as_bytes())?;
        return Ok(());
    }
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&section);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_private_atomic(path, text.as_bytes())?;
    Ok(())
}

fn remove_codex(path: &Path) -> Result<()> {
    let mut text = std::fs::read_to_string(path)?;
    if let Some((start, end)) = codex_section_range(&text) {
        text.replace_range(start..end, "");
        write_private_atomic(path, text.as_bytes())?;
    }
    Ok(())
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("configuration path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".panoptes-init-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = path
            .metadata()
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(0o600);
        options.mode(mode);
    }
    let write_result = (|| -> Result<()> {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    write_result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_supported_providers_use_user_level_paths() {
        let home = Path::new("/home/tester");
        for provider in PROVIDERS {
            let path = home.join(provider.config);
            assert!(path.starts_with(home));
        }
    }

    #[test]
    fn duplicate_provider_ids_are_applied_once() {
        let home =
            std::env::temp_dir().join(format!("panoptes-init-dedupe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let providers = ["codex".to_string(), "codex".to_string()];
        let writes = reconcile_at(&home, Path::new("/safe/panoptes"), &providers, true).unwrap();
        assert_eq!(writes.len(), 3);
        assert_eq!(
            writes.iter().filter(|write| write.format == "toml").count(),
            1
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn json_merge_preserves_foreign_servers() {
        let base = std::env::temp_dir().join(format!("panoptes-init-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join("mcp.json");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(&path, r#"{"mcpServers":{"other":{"command":"other"}}}"#).unwrap();
        merge_json(
            &path,
            "mcpServers",
            json!({"command":"/safe/panoptes", "args":["mcp"]}),
        )
        .unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["other"]["command"], "other");
        assert_eq!(value["mcpServers"]["panoptes"]["command"], "/safe/panoptes");
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn json_deregistration_preserves_foreign_servers() {
        let base =
            std::env::temp_dir().join(format!("panoptes-init-remove-json-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join(".claude.json");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"other"},"panoptes":{"command":"/safe/panoptes","args":["mcp"]}}}"#,
        )
        .unwrap();

        let writes = reconcile_at(&base, Path::new("/safe/panoptes"), &[], false).unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].action, "deregister");
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["other"]["command"], "other");
        assert!(value["mcpServers"].get("panoptes").is_none());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn codex_registration_updates_only_its_existing_section() {
        let base =
            std::env::temp_dir().join(format!("panoptes-init-toml-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("config.toml");
        std::fs::write(
            &path,
            "model = \"keep\"\n\n[mcp_servers.panoptes]\ncommand = \"/old/panoptes\"\nargs = [\"mcp\"]\n\n[mcp_servers.other]\ncommand = \"other\"\n",
        )
        .unwrap();
        upsert_codex(&path, Path::new("/new/panoptes")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("model = \"keep\""));
        assert!(text.contains("command = \"/new/panoptes\""));
        assert!(text.contains("[mcp_servers.other]\ncommand = \"other\""));
        assert!(!text.contains("/old/panoptes"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn codex_deregistration_removes_only_its_section() {
        let base =
            std::env::temp_dir().join(format!("panoptes-init-remove-toml-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let path = base.join(".codex/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "model = \"keep\"\n\n[mcp_servers.panoptes] # managed registration\ncommand = \"/safe/panoptes\"\nargs = [\"mcp\"]\n\n[mcp_servers.other]\ncommand = \"other\"\n",
        )
        .unwrap();
        let writes = reconcile_at(&base, Path::new("/safe/panoptes"), &[], false).unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].action, "deregister");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("model = \"keep\""));
        assert!(text.contains("[mcp_servers.other]\ncommand = \"other\""));
        assert!(!text.contains("[mcp_servers.panoptes]"));
        assert!(!text.contains("/safe/panoptes"));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn guidance_install_and_removal_preserve_existing_global_instructions() {
        let base = std::env::temp_dir().join(format!(
            "panoptes-init-guidance-preserve-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let agents = base.join(".codex/AGENTS.md");
        std::fs::create_dir_all(agents.parent().unwrap()).unwrap();
        std::fs::write(&agents, "# My instructions\n\nKeep this text.\n").unwrap();

        let selected = ["codex".to_string()];
        reconcile_at(&base, Path::new("/safe/panoptes"), &selected, false).unwrap();
        let installed = std::fs::read_to_string(&agents).unwrap();
        assert!(installed.contains("# My instructions"));
        assert!(installed.contains(GUIDANCE_START));
        assert_eq!(installed.matches(GUIDANCE_START).count(), 1);
        assert!(base.join(".agents/skills/panoptes/SKILL.md").exists());

        reconcile_at(&base, Path::new("/safe/panoptes"), &[], false).unwrap();
        let removed = std::fs::read_to_string(&agents).unwrap();
        assert!(removed.contains("# My instructions"));
        assert!(!removed.contains(GUIDANCE_START));
        assert!(!base.join(".agents/skills/panoptes/SKILL.md").exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn codex_guidance_uses_an_active_override_file() {
        let base = std::env::temp_dir().join(format!(
            "panoptes-init-guidance-override-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let override_path = base.join(".codex/AGENTS.override.md");
        std::fs::create_dir_all(override_path.parent().unwrap()).unwrap();
        std::fs::write(&override_path, "# Active override\n").unwrap();

        reconcile_at(
            &base,
            Path::new("/safe/panoptes"),
            &["codex".to_string()],
            false,
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(&override_path)
                .unwrap()
                .contains(GUIDANCE_START)
        );
        assert!(!base.join(".codex/AGENTS.md").exists());
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn every_provider_gets_a_supported_skill_location() {
        let base = std::env::temp_dir().join(format!(
            "panoptes-init-provider-skills-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let providers = PROVIDERS
            .iter()
            .map(|provider| provider.id.to_string())
            .collect::<Vec<_>>();
        let writes = reconcile_at(&base, Path::new("/safe/panoptes"), &providers, true).unwrap();
        let skill_paths = writes
            .iter()
            .filter(|write| write.format == "skill")
            .map(|write| write.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(skill_paths.len(), 3);
        assert!(
            skill_paths
                .iter()
                .any(|path| path.contains(".agents/skills"))
        );
        assert!(
            skill_paths
                .iter()
                .any(|path| path.contains(".claude/skills"))
        );
        assert!(
            skill_paths
                .iter()
                .any(|path| path.contains("antigravity-cli/skills"))
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn unmanaged_skill_is_never_overwritten_or_removed() {
        let base = std::env::temp_dir().join(format!(
            "panoptes-init-unmanaged-skill-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let skill = base.join(".agents/skills/panoptes/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(&skill, "user-owned skill\n").unwrap();

        let error = reconcile_at(
            &base,
            Path::new("/safe/panoptes"),
            &["codex".to_string()],
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("refusing to overwrite unmanaged"));
        reconcile_at(&base, Path::new("/safe/panoptes"), &[], false).unwrap();
        assert_eq!(
            std::fs::read_to_string(&skill).unwrap(),
            "user-owned skill\n"
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
