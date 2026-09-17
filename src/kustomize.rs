//! Kustomize's source graph, without running transformers or generators.
use crate::ansible::Resolved;
use crate::extract::Extracted;
use crate::index::Pending;
use crate::kubernetes::Resource;
use crate::yaml::{self, Dialect, Link, Target, Yaml};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::Node;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Selector {
    pub fields: Vec<(String, String)>,
    pub labels: Option<String>,
    pub unsupported: bool,
}
fn selector<'a>(node: Node<'a>, yaml: &Yaml<'a, '_>) -> Selector {
    let mut result = Selector::default();
    for (key, value) in yaml::pairs(node, yaml) {
        if key == "labelSelector" {
            result.labels = yaml::literal(value, yaml);
            result.unsupported |= result.labels.is_none();
        } else if matches!(
            key.as_str(),
            "group" | "version" | "kind" | "name" | "namespace"
        ) {
            if let Some(value) = yaml::scalar(value, yaml).filter(|s| !s.contains("{{")) {
                result.fields.push((key, value));
            } else {
                result.unsupported = true;
            }
        } else {
            result.unsupported = true;
        }
    }
    result
}
fn link(ex: &mut Extracted, from: Option<usize>, target: Target) {
    ex.automation.links.push(Link {
        from,
        scope: None,
        target,
    });
}
fn local(base: &str, spec: &str) -> Option<String> {
    if spec.contains("://") || spec.contains("?ref=") || spec.contains("::") || spec.contains('$') {
        return None;
    }
    yaml::path(base, spec)
}
fn candidates(path: String) -> Vec<String> {
    vec![
        path.clone(),
        format!("{path}/kustomization.yaml"),
        format!("{path}/kustomization.yml"),
        format!("{path}/Kustomization"),
    ]
}
pub fn enrich<'a>(root: Node<'a>, yaml: &Yaml<'a, '_>, path: &str, ex: &mut Extracted) {
    if !matches!(
        Path::new(path).file_name().and_then(|s| s.to_str()),
        Some("kustomization.yml" | "kustomization.yaml" | "Kustomization")
    ) {
        return;
    }
    let base = Path::new(path)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or("");
    ex.automation.dialect = Dialect::Kustomize;
    for doc in yaml::children(root).filter(|n| n.kind() == "document") {
        for key in ["resources", "bases", "components"] {
            if let Some(entries) = yaml::get(doc, yaml, key) {
                for entry in yaml::items(entries, yaml) {
                    if let Some(path) = yaml::literal(entry, yaml).and_then(|s| local(base, &s)) {
                        link(ex, None, Target::KustomizeResource(candidates(path)));
                    }
                }
            }
        }
        for key in ["configurations", "crds", "transformers", "generators"] {
            if let Some(entries) = yaml::get(doc, yaml, key) {
                for entry in yaml::items(entries, yaml) {
                    if let Some(path) = yaml::literal(entry, yaml).and_then(|s| local(base, &s)) {
                        link(ex, None, Target::File(vec![path]));
                    }
                }
            }
        }
        for key in ["patches", "patchesStrategicMerge", "patchesJson6902"] {
            if let Some(entries) = yaml::get(doc, yaml, key) {
                for (n, entry) in yaml::items(entries, yaml).into_iter().enumerate() {
                    let file = yaml::get(entry, yaml, "path")
                        .or_else(|| (key == "patchesStrategicMerge").then_some(entry));
                    let path = file
                        .and_then(|n| yaml::literal(n, yaml))
                        .and_then(|s| local(base, &s));
                    let target = yaml::get(entry, yaml, "target").map(|n| selector(n, yaml));
                    let id = yaml::symbol(
                        ex,
                        yaml,
                        entry,
                        format!("patch: {key}#{}", n + 1),
                        "patch",
                        None,
                    );
                    link(ex, None, Target::Symbol(id));
                    link(
                        ex,
                        Some(id),
                        Target::Patch {
                            path,
                            selector: target,
                        },
                    );
                }
            }
        }
        for key in ["configMapGenerator", "secretGenerator"] {
            if let Some(entries) = yaml::get(doc, yaml, key) {
                for entry in yaml::items(entries, yaml) {
                    let Some(name) =
                        yaml::get(entry, yaml, "name").and_then(|n| yaml::literal(n, yaml))
                    else {
                        continue;
                    };
                    let id = yaml::symbol(
                        ex,
                        yaml,
                        entry,
                        format!("generator: {name}"),
                        "generator",
                        None,
                    );
                    link(ex, None, Target::Symbol(id));
                    for key in ["files", "envs"] {
                        if let Some(inputs) = yaml::get(entry, yaml, key) {
                            for input in yaml::strings(inputs, yaml) {
                                let file = if key == "files" {
                                    input.split_once('=').map_or(input.as_str(), |(_, p)| p)
                                } else {
                                    &input
                                };
                                if let Some(path) = local(base, file) {
                                    link(ex, Some(id), Target::File(vec![path]));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn matches(
    selector: &Selector,
    resource: &Resource,
    cache: &mut HashMap<String, Option<regex::Regex>>,
) -> bool {
    if selector.unsupported {
        return false;
    }
    for (key, pattern) in &selector.fields {
        let value = match key.as_str() {
            "group" => &resource.key.group,
            "version" => &resource.version,
            "kind" => &resource.key.kind,
            "name" => &resource.key.name,
            "namespace" => &resource.key.namespace,
            _ => return false,
        };
        if pattern.is_empty() {
            continue;
        }
        let regex = cache
            .entry(pattern.clone())
            .or_insert_with(|| regex::Regex::new(&format!("^(?:{pattern})$")).ok());
        let Some(regex) = regex else {
            return false;
        };
        if !regex.is_match(value) {
            return false;
        }
    }
    if let Some(selector) = &selector.labels {
        // Equality and existence selectors only; set-based and annotation
        // selectors remain unresolved instead of broadening their match.
        for term in selector.split(',').map(str::trim) {
            if term.contains(['(', ')']) || term.is_empty() {
                return false;
            }
            let (key, expected, negated) = if let Some((k, v)) = term.split_once("!=") {
                (k.trim(), Some(v.trim()), true)
            } else if let Some((k, v)) = term.split_once("==").or_else(|| term.split_once('=')) {
                (k.trim(), Some(v.trim()), false)
            } else if let Some(k) = term.strip_prefix('!') {
                (k, None, true)
            } else {
                (term, None, false)
            };
            let actual = resource
                .labels
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str());
            let equal = expected.map_or(actual.is_some(), |want| actual == Some(want));
            if equal == negated {
                return false;
            }
        }
    }
    true
}

pub fn patch_only_paths(pending: &[Pending]) -> HashSet<String> {
    let mut patches = HashSet::new();
    let mut resources = HashSet::new();
    for file in pending {
        for link in &file.extracted.automation.links {
            match &link.target {
                Target::Patch {
                    path: Some(path), ..
                } => {
                    patches.insert(path.clone());
                }
                Target::KustomizeResource(paths) => resources.extend(paths.iter().cloned()),
                _ => {}
            }
        }
    }
    patches.retain(|p| !resources.contains(p));
    patches
}

pub fn resolve(pending: &[Pending], files: &HashMap<String, i64>) -> Resolved {
    let mut regex_cache = HashMap::new();
    let by_path: HashMap<_, _> = pending.iter().map(|f| (f.rel.as_str(), f)).collect();
    let mut result = Resolved {
        edges: Vec::new(),
        unresolved: 0,
    };
    for file in pending
        .iter()
        .filter(|f| f.extracted.automation.dialect == Dialect::Kustomize)
    {
        let mut reachable = HashSet::new();
        let mut todo = vec![file.rel.as_str()];
        while let Some(path) = todo.pop() {
            if reachable.insert(path)
                && let Some(file) = by_path.get(path)
            {
                for link in &file.extracted.automation.links {
                    if let Target::KustomizeResource(paths) = &link.target
                        && let Some(target) = paths.iter().find(|p| files.contains_key(*p))
                    {
                        todo.push(target);
                    }
                }
            }
        }
        for link in &file.extracted.automation.links {
            let from = link
                .from
                .map(|i| file.symbol_ids[i])
                .unwrap_or(file.file_symbol);
            match &link.target {
                Target::Symbol(i) => result.edges.push((from, file.symbol_ids[*i], "calls")),
                Target::File(paths) | Target::KustomizeResource(paths) => {
                    if let Some(id) = paths.iter().find_map(|p| files.get(p)) {
                        result.edges.push((from, *id, "imports"));
                    } else {
                        result.unresolved += 1;
                    }
                }
                Target::Patch { path, selector } => {
                    let patch = path.as_deref().and_then(|p| by_path.get(p));
                    if let Some(patch) = patch {
                        result.edges.push((from, patch.file_symbol, "imports"));
                    }
                    let inferred = patch
                        .into_iter()
                        .flat_map(|f| &f.extracted.automation.resources)
                        .map(|r| Selector {
                            fields: vec![
                                (
                                    "group".into(),
                                    format!("(?:{})", regex::escape(&r.key.group)),
                                ),
                                ("version".into(), regex::escape(&r.version)),
                                ("kind".into(), regex::escape(&r.key.kind)),
                                ("name".into(), regex::escape(&r.key.name)),
                                ("namespace".into(), regex::escape(&r.key.namespace)),
                            ],
                            ..Selector::default()
                        })
                        .collect::<Vec<_>>();
                    let selectors = selector
                        .as_ref()
                        .map(std::slice::from_ref)
                        .unwrap_or(&inferred);
                    let mut found = false;
                    for path in &reachable {
                        if let Some(target) = by_path.get(path) {
                            for resource in &target.extracted.automation.resources {
                                if selectors
                                    .iter()
                                    .any(|s| matches(s, resource, &mut regex_cache))
                                {
                                    result.edges.push((
                                        from,
                                        target.symbol_ids[resource.symbol],
                                        "calls",
                                    ));
                                    found = true;
                                }
                            }
                        }
                    }
                    if !found {
                        result.unresolved += 1;
                    }
                }
                _ => {}
            }
        }
    }
    result.edges.sort_unstable();
    result.edges.dedup();
    result
}
