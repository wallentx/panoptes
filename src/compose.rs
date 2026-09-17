//! Docker Compose resources and explicitly declared project dependencies.
use crate::ansible::Resolved;
use crate::extract::Extracted;
use crate::index::Pending;
use crate::yaml::{self, Dialect, Link, Target};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::Node;

fn literal(node: Node<'_>, src: &str) -> Option<String> {
    yaml::literal(node, src).filter(|s| !s.contains('$'))
}
fn link(ex: &mut Extracted, from: Option<usize>, target: Target) {
    ex.automation.links.push(Link {
        from,
        scope: None,
        target,
    });
}
fn local(ex: &mut Extracted, from: usize, kind: &str, name: &str) {
    if !name.is_empty() && !name.contains('$') {
        link(ex, Some(from), Target::Local(format!("{kind}: {name}")));
    }
}
fn names(node: Node<'_>, src: &str) -> Vec<String> {
    let pairs = yaml::pairs(node, src);
    if pairs.is_empty() {
        yaml::items(node)
            .into_iter()
            .filter_map(|n| literal(n, src))
            .collect()
    } else {
        pairs.into_iter().map(|(k, _)| k).collect()
    }
}

pub fn enrich(root: Node<'_>, src: &str, path: &str, ex: &mut Extracted) {
    if ex.automation.dialect != Dialect::Generic {
        return;
    }
    let base = Path::new(path)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or("");
    let filename = Path::new(path)
        .file_stem()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    let conventional = filename == "compose"
        || filename.starts_with("compose.")
        || filename == "docker-compose"
        || filename.starts_with("docker-compose.");
    for doc in yaml::children(root).filter(|n| n.kind() == "document") {
        let top = yaml::unwrap(doc);
        let services = yaml::get(top, src, "services");
        let recognizable = services.is_some_and(|n| {
            yaml::pairs(n, src).iter().any(|(_, service)| {
                yaml::pairs(*service, src).iter().any(|(k, _)| {
                    matches!(k.as_str(), "image" | "build" | "extends" | "depends_on")
                })
            })
        });
        if !conventional && !recognizable {
            continue;
        }
        ex.automation.dialect = Dialect::Compose;
        for (key, kind) in [
            ("networks", "network"),
            ("volumes", "volume"),
            ("configs", "config"),
            ("secrets", "secret"),
        ] {
            if let Some(resources) = yaml::get(top, src, key) {
                for (name, value) in yaml::pairs(resources, src) {
                    let id = yaml::symbol(ex, src, value, format!("{kind}: {name}"), kind, None);
                    if matches!(key, "configs" | "secrets")
                        && let Some(file) =
                            yaml::get(value, src, "file").and_then(|n| literal(n, src))
                        && let Some(path) = yaml::path(base, &file)
                    {
                        link(ex, Some(id), Target::File(vec![path]));
                    }
                }
            }
        }
        if let Some(includes) = yaml::get(top, src, "include") {
            for item in yaml::items(includes) {
                let value = yaml::get(item, src, "path").unwrap_or(item);
                let paths = literal(value, src)
                    .map(|s| vec![s])
                    .unwrap_or_else(|| names(value, src));
                for file in paths {
                    if let Some(path) = yaml::path(base, &file) {
                        link(ex, None, Target::File(vec![path]));
                    }
                }
            }
        }
        if let Some(services) = services {
            for (name, service) in yaml::pairs(services, src) {
                let id = yaml::symbol(
                    ex,
                    src,
                    service,
                    format!("service: {name}"),
                    "service",
                    None,
                );
                link(ex, None, Target::Symbol(id));
                for (key, value) in yaml::pairs(service, src) {
                    match key.as_str() {
                        "depends_on" | "networks" => {
                            let kind = if key == "depends_on" {
                                "service"
                            } else {
                                "network"
                            };
                            for name in names(value, src) {
                                local(ex, id, kind, &name);
                            }
                        }
                        "links" | "volumes_from" => {
                            for spec in names(value, src) {
                                if !spec.starts_with("container:") {
                                    local(ex, id, "service", spec.split(':').next().unwrap_or(""));
                                }
                            }
                        }
                        "network_mode" | "ipc" | "pid" => {
                            if let Some(spec) = literal(value, src)
                                .and_then(|s| s.strip_prefix("service:").map(str::to_string))
                            {
                                local(ex, id, "service", &spec);
                            }
                        }
                        "configs" | "secrets" => {
                            for entry in yaml::items(value) {
                                let source = yaml::get(entry, src, "source").unwrap_or(entry);
                                if let Some(name) = literal(source, src) {
                                    local(
                                        ex,
                                        id,
                                        if key == "configs" { "config" } else { "secret" },
                                        &name,
                                    );
                                }
                            }
                        }
                        "volumes" => {
                            for entry in yaml::items(value) {
                                let source = if let Some(source) = yaml::get(entry, src, "source") {
                                    let volume = yaml::get(entry, src, "type")
                                        .and_then(|n| literal(n, src))
                                        .as_deref()
                                        == Some("volume");
                                    if volume { literal(source, src) } else { None }
                                } else {
                                    literal(entry, src).and_then(|s| {
                                        s.split_once(':').map(|(source, _)| source.to_string())
                                    })
                                };
                                if let Some(name) = source
                                    && !name.contains(['/', '\\', ':'])
                                    && !name.starts_with(['.', '~'])
                                {
                                    local(ex, id, "volume", &name);
                                }
                            }
                        }
                        "extends" => {
                            if let Some(name) =
                                literal(yaml::get(value, src, "service").unwrap_or(value), src)
                            {
                                if let Some(file) = yaml::get(value, src, "file") {
                                    if let Some(path) =
                                        literal(file, src).and_then(|p| yaml::path(base, &p))
                                    {
                                        link(
                                            ex,
                                            Some(id),
                                            Target::Named {
                                                path,
                                                name: format!("service: {name}"),
                                            },
                                        );
                                    }
                                } else {
                                    local(ex, id, "service", &name);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

pub fn resolve(pending: &[Pending], files: &HashMap<String, i64>) -> Resolved {
    let compose: HashMap<_, _> = pending
        .iter()
        .filter(|f| f.extracted.automation.dialect == Dialect::Compose)
        .map(|f| (f.rel.as_str(), f))
        .collect();
    let mut result = Resolved {
        edges: Vec::new(),
        unresolved: 0,
    };
    let mut names = HashMap::<(&str, &str), Vec<i64>>::new();
    for file in compose.values() {
        for (i, s) in file.extracted.symbols.iter().enumerate() {
            names
                .entry((&file.rel, &s.name))
                .or_default()
                .push(file.symbol_ids[i]);
        }
    }
    for file in compose.values() {
        let mut visible = HashSet::new();
        let mut todo = vec![file.rel.as_str()];
        while let Some(path) = todo.pop() {
            if !visible.insert(path) {
                continue;
            }
            if let Some(included) = compose.get(path) {
                for link in &included.extracted.automation.links {
                    if link.from.is_none()
                        && let Target::File(paths) = &link.target
                    {
                        todo.extend(
                            paths
                                .iter()
                                .map(String::as_str)
                                .filter(|p| compose.contains_key(p)),
                        );
                    }
                }
            }
        }
        for link in &file.extracted.automation.links {
            let from = link
                .from
                .map(|i| file.symbol_ids[i])
                .unwrap_or(file.file_symbol);
            let (mut targets, kind) = match &link.target {
                Target::Symbol(i) => (vec![file.symbol_ids[*i]], "calls"),
                Target::Local(name) => (
                    visible
                        .iter()
                        .flat_map(|p| {
                            names
                                .get(&(*p, name.as_str()))
                                .into_iter()
                                .flatten()
                                .copied()
                        })
                        .collect(),
                    "calls",
                ),
                Target::Named { path, name } => (
                    names
                        .get(&(path.as_str(), name.as_str()))
                        .cloned()
                        .unwrap_or_default(),
                    "calls",
                ),
                Target::File(paths) => (
                    paths
                        .iter()
                        .find_map(|p| files.get(p).copied())
                        .into_iter()
                        .collect(),
                    "imports",
                ),
                _ => continue,
            };
            targets.sort_unstable();
            targets.dedup();
            if let [target] = targets.as_slice() {
                result.edges.push((from, *target, kind));
            } else {
                result.unresolved += 1;
            }
        }
    }
    result.edges.sort_unstable();
    result.edges.dedup();
    result
}
