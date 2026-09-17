//! GitLab CI includes and job dependencies, scoped to declared pipeline sources.
use crate::ansible::Resolved;
use crate::extract::Extracted;
use crate::index::Pending;
use crate::yaml::{self, Dialect, Link, Target, Yaml};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::Node;
fn literal<'a>(n: Node<'a>, yaml: &Yaml<'a, '_>) -> Option<String> {
    yaml::literal(n, yaml).filter(|s| !s.contains('$'))
}
fn link(ex: &mut Extracted, from: Option<usize>, target: Target) {
    ex.automation.links.push(Link {
        from,
        scope: None,
        target,
    });
}
fn job(ex: &mut Extracted, from: usize, name: String) {
    link(ex, Some(from), Target::Local(format!("gitlab job: {name}")));
}
fn reserved(name: &str) -> bool {
    matches!(
        name,
        "include"
            | "stages"
            | "default"
            | "variables"
            | "workflow"
            | "image"
            | "services"
            | "cache"
            | "before_script"
            | "after_script"
            | "spec"
            | "types"
    )
}
fn root_file(path: &str) -> bool {
    matches!(path, ".gitlab-ci.yml" | ".gitlab-ci.yaml")
}
fn external(ex: &mut Extracted, from: Option<usize>, name: String) {
    ex.imports.push(name.clone());
    link(ex, from, Target::External(name));
}
fn include<'a>(ex: &mut Extracted, from: Option<usize>, node: Node<'a>, yaml: &Yaml<'a, '_>) {
    let mut todo = vec![node];
    let mut seen = HashSet::new();
    while let Some(node) = todo.pop() {
        let node = yaml::resolve(node, yaml);
        if !seen.insert(node.id()) {
            continue;
        }
        let sequence = yaml::items(node, yaml);
        if sequence.is_empty() {
            include_item(ex, from, node, yaml);
        } else {
            todo.extend(sequence.into_iter().rev());
        }
    }
}
fn include_item<'a>(ex: &mut Extracted, from: Option<usize>, node: Node<'a>, yaml: &Yaml<'a, '_>) {
    if let Some(project) = yaml::get(node, yaml, "project").and_then(|n| literal(n, yaml)) {
        let reference = match yaml::get(node, yaml, "ref") {
            Some(n) => literal(n, yaml),
            None => Some("HEAD".to_string()),
        };
        if let (Some(reference), Some(files)) = (reference, yaml::get(node, yaml, "file")) {
            for file in yaml::strings(files, yaml)
                .into_iter()
                .filter(|s| !s.contains('$'))
            {
                external(ex, from, format!("gitlab:{project}@{reference}:{file}"));
            }
        }
        return;
    }
    for key in ["remote", "template", "component"] {
        if let Some(value) = yaml::get(node, yaml, key).and_then(|n| literal(n, yaml)) {
            external(ex, from, format!("gitlab:{key}:{value}"));
            return;
        }
    }
    let value = yaml::get(node, yaml, "local").unwrap_or(node);
    if let Some(value) = literal(value, yaml) {
        if value.starts_with("https://") || value.starts_with("http://") {
            external(ex, from, format!("gitlab:remote:{value}"));
        } else if !value.contains(['*', '?', '['])
            && let Some(path) = yaml::path("", value.trim_start_matches('/'))
        {
            link(ex, from, Target::File(vec![path]));
        }
    }
}
fn references<'a>(
    ex: &mut Extracted,
    id: usize,
    node: Node<'a>,
    yaml: &Yaml<'a, '_>,
    depth: usize,
    seen: &mut HashMap<usize, usize>,
) {
    if depth >= 64 {
        return;
    }
    let node = yaml::dereference(node, yaml);
    if yaml::children(node).any(|n| n.kind() == "tag" && &yaml[n.byte_range()] == "!reference") {
        if let Some(name) = yaml::items(node, yaml)
            .first()
            .and_then(|&n| literal(n, yaml))
        {
            job(ex, id, name);
        }
        return;
    }
    if matches!(
        node.kind(),
        "block_node" | "flow_node" | "block_sequence_item"
    ) {
        for child in yaml::children(node) {
            references(ex, id, child, yaml, depth + 1, seen);
        }
        return;
    }
    let node = yaml::resolve(node, yaml);
    if seen
        .get(&node.id())
        .is_some_and(|&previous| previous <= depth)
    {
        return;
    }
    seen.insert(node.id(), depth);
    for (_, value) in yaml::pairs(node, yaml) {
        references(ex, id, value, yaml, depth + 1, seen);
    }
    if matches!(node.kind(), "block_sequence" | "flow_sequence") {
        for item in yaml::children(node) {
            references(ex, id, item, yaml, depth + 1, seen);
        }
    }
}
pub fn enrich<'a>(
    root: Node<'a>,
    yaml: &Yaml<'a, '_>,
    path: &str,
    ex: &mut Extracted,
    included: bool,
) {
    if ex.automation.dialect != Dialect::Generic {
        return;
    }
    // A nested conventional filename supplies no root-pipeline context. It can
    // still be activated by a local include or a child-pipeline trigger.
    if !included
        && !root_file(path)
        && Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(root_file)
    {
        return;
    }
    for doc in yaml::children(root).filter(|n| n.kind() == "document") {
        let pairs = yaml::pairs(doc, yaml);
        let recognized = root_file(path)
            || pairs.iter().any(|(name, value)| {
                name == "include"
                    || name == "default"
                    || (!reserved(name)
                        && yaml::pairs(*value, yaml).iter().any(|(key, _)| {
                            matches!(key.as_str(), "script" | "extends" | "trigger" | "needs")
                        }))
            });
        if !recognized && !included {
            continue;
        }
        ex.automation.dialect = Dialect::GitLab;
        ex.automation.gitlab_included = included;
        if let Some(entries) = yaml::get(doc, yaml, "include") {
            include(ex, None, entries, yaml);
        }
        if let Some(stages) = yaml::get(doc, yaml, "stages") {
            for stage in yaml::items(stages, yaml) {
                if let Some(name) = literal(stage, yaml) {
                    yaml::symbol(
                        ex,
                        yaml,
                        stage,
                        format!("gitlab stage: {name}"),
                        "stage",
                        None,
                    );
                }
            }
        }
        if let Some(default) = yaml::get(doc, yaml, "default") {
            let id = yaml::symbol(ex, yaml, default, "gitlab default".into(), "template", None);
            references(ex, id, default, yaml, 0, &mut HashMap::new());
        }
        for (name, value) in pairs.into_iter().filter(|(name, _)| !reserved(name)) {
            let fields = yaml::pairs(value, yaml);
            if !matches!(
                yaml::resolve(value, yaml).kind(),
                "block_mapping" | "flow_mapping"
            ) || (!root_file(path)
                && !included
                && !fields.iter().any(|(key, _)| {
                    matches!(
                        key.as_str(),
                        "script"
                            | "extends"
                            | "trigger"
                            | "needs"
                            | "stage"
                            | "rules"
                            | "dependencies"
                            | "before_script"
                            | "after_script"
                            | "variables"
                            | "image"
                            | "artifacts"
                    )
                }))
            {
                continue;
            }
            let id = yaml::symbol(
                ex,
                yaml,
                value,
                format!("gitlab job: {name}"),
                if name.starts_with('.') {
                    "template"
                } else {
                    "job"
                },
                None,
            );
            if !name.starts_with('.') {
                link(ex, None, Target::Symbol(id));
            }
            for key in ["extends", "dependencies"] {
                if let Some(targets) = yaml::get(value, yaml, key) {
                    for name in yaml::strings(targets, yaml)
                        .into_iter()
                        .filter(|s| !s.contains('$'))
                    {
                        job(ex, id, name);
                    }
                }
            }
            if let Some(needs) = yaml::get(value, yaml, "needs") {
                for need in yaml::items(needs, yaml) {
                    if yaml::get(need, yaml, "project").is_some()
                        || yaml::get(need, yaml, "pipeline").is_some()
                    {
                        continue;
                    }
                    if let Some(name) = literal(yaml::get(need, yaml, "job").unwrap_or(need), yaml)
                    {
                        job(ex, id, name);
                    }
                }
            }
            if let Some(stage) = yaml::get(value, yaml, "stage").and_then(|n| literal(n, yaml)) {
                link(
                    ex,
                    Some(id),
                    Target::Local(format!("gitlab stage: {stage}")),
                );
            }
            if let Some(trigger) = yaml::get(value, yaml, "trigger") {
                if let Some(child) = yaml::get(trigger, yaml, "include") {
                    include(ex, Some(id), child, yaml);
                }
                if let Some(project) =
                    yaml::get(trigger, yaml, "project").and_then(|n| literal(n, yaml))
                {
                    external(ex, Some(id), format!("gitlab:pipeline:{project}"));
                }
            }
            let inherit = yaml::get(value, yaml, "inherit")
                .and_then(|n| yaml::get(n, yaml, "default"))
                .and_then(|n| yaml::scalar(n, yaml));
            if inherit.as_deref() != Some("false") {
                link(ex, Some(id), Target::Local("gitlab default".into()));
            }
            references(ex, id, value, yaml, 0, &mut HashMap::new());
        }
    }
}

pub fn resolve(
    pending: &[Pending],
    files: &HashMap<String, i64>,
    external: &HashMap<String, i64>,
) -> Resolved {
    let by_path: HashMap<_, _> = pending
        .iter()
        .filter(|f| f.extracted.automation.dialect == Dialect::GitLab)
        .map(|f| (f.rel.as_str(), f))
        .collect();
    fn closure<'a>(path: &'a str, files: &HashMap<&'a str, &'a Pending>) -> HashSet<&'a str> {
        let mut seen = HashSet::new();
        let mut todo = vec![path];
        while let Some(path) = todo.pop() {
            if seen.insert(path)
                && let Some(file) = files.get(path)
            {
                for link in &file.extracted.automation.links {
                    if link.from.is_none()
                        && let Target::File(paths) = &link.target
                    {
                        todo.extend(
                            paths
                                .iter()
                                .map(String::as_str)
                                .filter(|p| files.contains_key(p)),
                        );
                    }
                }
            }
        }
        seen
    }
    // A child pipeline is its own root; its jobs never enter its parent's scope.
    let mut roots: HashSet<_> = by_path.keys().copied().filter(|p| root_file(p)).collect();
    for file in by_path.values() {
        for link in &file.extracted.automation.links {
            if link.from.is_some()
                && let Target::File(paths) = &link.target
            {
                roots.extend(
                    paths
                        .iter()
                        .map(String::as_str)
                        .filter(|p| by_path.contains_key(p)),
                );
            }
        }
    }
    let contexts: Vec<_> = roots.iter().map(|&p| closure(p, &by_path)).collect();
    let mut result = Resolved {
        edges: Vec::new(),
        unresolved: 0,
    };
    for file in by_path.values() {
        let local = closure(&file.rel, &by_path);
        let mut scopes: Vec<_> = contexts
            .iter()
            .filter(|set| set.contains(file.rel.as_str()))
            .collect();
        if scopes.is_empty() {
            if !root_file(&file.rel)
                && !file
                    .extracted
                    .symbols
                    .iter()
                    .any(|s| s.name.starts_with("gitlab job:"))
            {
                continue;
            }
            scopes.push(&local);
        }
        for link in &file.extracted.automation.links {
            let from = link
                .from
                .map(|i| file.symbol_ids[i])
                .unwrap_or(file.file_symbol);
            match &link.target {
                Target::Symbol(i) => result.edges.push((from, file.symbol_ids[*i], "calls")),
                Target::Local(name) => {
                    let mut targets = HashSet::new();
                    for scope in &scopes {
                        let ids: Vec<_> = scope
                            .iter()
                            .filter_map(|p| by_path.get(p))
                            .flat_map(|f| {
                                f.extracted
                                    .symbols
                                    .iter()
                                    .enumerate()
                                    .filter(|(_, s)| s.name == *name)
                                    .map(|(i, _)| f.symbol_ids[i])
                            })
                            .collect();
                        if let [id] = ids.as_slice() {
                            targets.insert(*id);
                        }
                    }
                    if targets.is_empty() && name != "gitlab default" {
                        result.unresolved += 1;
                    }
                    for target in targets {
                        result.edges.push((from, target, "calls"));
                    }
                }
                Target::File(paths) => {
                    if let Some(id) = paths.iter().find_map(|p| files.get(p)) {
                        result.edges.push((from, *id, "imports"));
                    } else {
                        result.unresolved += 1;
                    }
                }
                Target::External(name) => {
                    if let Some(id) = external.get(name) {
                        result.edges.push((from, *id, "imports"));
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

fn pipeline_seed(file: &Pending) -> bool {
    file.extracted.automation.dialect == Dialect::GitLab
        && !file.extracted.automation.gitlab_included
        && (root_file(&file.rel)
            || file
                .extracted
                .symbols
                .iter()
                .any(|s| s.name.starts_with("gitlab job:")))
}

/// Infer format only for files reached from a local pipeline include. Replay
/// unchanged inferred payloads, and remove inferred symbols when the include
/// disappears, even if the fragment's own content has not changed.
pub fn contextualize(
    pending: &mut [Pending],
    sources: &[crate::repo::SourceFile],
) -> anyhow::Result<Vec<usize>> {
    let indexes: HashMap<_, _> = sources
        .iter()
        .enumerate()
        .map(|(i, f)| (f.rel.as_str(), i))
        .collect();
    let mut todo: Vec<_> = pending
        .iter()
        .enumerate()
        .filter(|(_, f)| pipeline_seed(f))
        .map(|(i, _)| i)
        .collect();
    let mut seen = HashSet::new();
    let mut changed = Vec::new();
    let mut extractor = crate::extract::Extractor::new();
    while let Some(index) = todo.pop() {
        if !seen.insert(index) || pending[index].lang != crate::repo::Lang::Yaml {
            continue;
        }
        let file = &mut pending[index];
        if file.extracted.automation.dialect == Dialect::Generic {
            file.extracted = extractor.extract_gitlab_include(&sources[index].text, &file.rel)?;
            changed.push(index);
        }
        if file.extracted.automation.dialect != Dialect::GitLab {
            continue;
        }
        for link in &file.extracted.automation.links {
            if let Target::File(paths) = &link.target {
                todo.extend(
                    paths
                        .iter()
                        .filter_map(|p| indexes.get(p.as_str()).copied()),
                );
            }
        }
    }
    for (index, file) in pending.iter_mut().enumerate() {
        if file.extracted.automation.gitlab_included && !seen.contains(&index) {
            file.extracted = extractor.extract_file(file.lang, &sources[index].text, &file.rel)?;
            changed.push(index);
        }
    }
    changed.sort_unstable();
    Ok(changed)
}

/// Include-only fragments are active only when reached from a pipeline/job file.
pub fn active_paths(pending: &[Pending]) -> HashSet<&str> {
    let files: HashMap<_, _> = pending
        .iter()
        .filter(|f| f.extracted.automation.dialect == Dialect::GitLab)
        .map(|f| (f.rel.as_str(), f))
        .collect();
    let mut todo: Vec<_> = files
        .values()
        .filter(|f| pipeline_seed(f))
        .map(|f| f.rel.as_str())
        .collect();
    let mut active = HashSet::new();
    while let Some(path) = todo.pop() {
        if active.insert(path)
            && let Some(file) = files.get(path)
        {
            for link in &file.extracted.automation.links {
                if let Target::File(paths) = &link.target {
                    todo.extend(
                        paths
                            .iter()
                            .map(String::as_str)
                            .filter(|p| files.contains_key(p)),
                    );
                }
            }
        }
    }
    active
}
