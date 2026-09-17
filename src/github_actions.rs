//! Static GitHub Actions jobs, composite steps, uses, and expression references.
use crate::ansible::Resolved;
use crate::extract::Extracted;
use crate::index::Pending;
use crate::yaml::{self, Link, Target};
use std::collections::HashMap;
use std::path::Path;
use tree_sitter::Node;

fn link(ex: &mut Extracted, from: Option<usize>, target: Target) {
    ex.automation.links.push(Link {
        from,
        scope: None,
        target,
    });
}

pub fn enrich(root: Node<'_>, src: &str, path: &str, ex: &mut Extracted) {
    let workflow = Path::new(path).parent() == Some(Path::new(".github/workflows"));
    let action = matches!(
        Path::new(path).file_name().and_then(|p| p.to_str()),
        Some("action.yml" | "action.yaml")
    );
    if !workflow && !action {
        return;
    }
    for doc in yaml::children(root).filter(|n| n.kind() == "document") {
        let top = yaml::unwrap(doc);
        if !workflow && yaml::get(top, src, "runs").is_none() {
            continue;
        }
        ex.automation.dialect = yaml::Dialect::GitHubActions;
        if workflow {
            if let Some(on) = yaml::get(top, src, "on") {
                for trigger in ["workflow_call", "workflow_dispatch"] {
                    if let Some(config) = yaml::get(on, src, trigger) {
                        definitions(ex, src, config, "inputs", "input", None, None);
                        if trigger == "workflow_call" {
                            definitions(ex, src, config, "outputs", "output: workflow", None, None);
                        }
                    }
                }
            }
            if let Some(jobs) = yaml::get(top, src, "jobs") {
                for (name, job) in yaml::pairs(jobs, src) {
                    let job_id = yaml::symbol(ex, src, job, format!("job: {name}"), "job", None);
                    link(ex, None, Target::Symbol(job_id));
                    if let Some(needs) = yaml::get(job, src, "needs") {
                        for target in yaml::strings(needs, src) {
                            link(ex, Some(job_id), Target::Local(format!("job: {target}")));
                        }
                    }
                    if let Some(uses) = yaml::get(job, src, "uses") {
                        uses_link(ex, src, uses, job_id, true);
                    }
                    definitions(
                        ex,
                        src,
                        job,
                        "outputs",
                        &format!("output: job.{name}"),
                        Some(job_id),
                        Some(&name),
                    );
                    if let Some(steps) = yaml::get(job, src, "steps") {
                        step_list(ex, src, steps, Some(job_id), Some(&name));
                    }
                    scan_mapping(
                        ex,
                        src,
                        job,
                        Some(job_id),
                        Some(&name),
                        &["steps", "outputs", "needs", "uses"],
                    );
                }
            }
            scan_mapping(ex, src, top, None, None, &["jobs", "on"]);
        } else {
            definitions(ex, src, top, "inputs", "input", None, None);
            definitions(ex, src, top, "outputs", "output: action", None, None);
            if let Some(runs) = yaml::get(top, src, "runs") {
                let using = yaml::get(runs, src, "using").and_then(|n| yaml::literal(n, src));
                if using.as_deref() == Some("composite") {
                    if let Some(steps) = yaml::get(runs, src, "steps") {
                        step_list(ex, src, steps, None, None);
                    }
                } else if using.as_deref().is_some_and(|v| v.starts_with("node")) {
                    // JavaScript action entry points are relative to action.yml.
                    let base = Path::new(path)
                        .parent()
                        .and_then(Path::to_str)
                        .unwrap_or("");
                    for key in ["pre", "main", "post"] {
                        if let Some(value) =
                            yaml::get(runs, src, key).and_then(|n| yaml::literal(n, src))
                            && let Some(target) = yaml::path(base, &value)
                        {
                            link(ex, None, Target::File(vec![target]));
                        }
                    }
                }
            }
        }
    }
}

fn definitions(
    ex: &mut Extracted,
    src: &str,
    node: Node<'_>,
    key: &str,
    prefix: &str,
    parent: Option<usize>,
    job: Option<&str>,
) {
    if let Some(defs) = yaml::get(node, src, key) {
        for (name, value) in yaml::pairs(defs, src) {
            let full = format!("{prefix}.{name}");
            // A workflow may declare the same input for dispatch and call.
            let id = if prefix == "input" {
                ex.symbols
                    .iter()
                    .position(|s| s.name == full && s.kind == "input")
            } else {
                None
            }
            .unwrap_or_else(|| {
                yaml::symbol(
                    ex,
                    src,
                    value,
                    full,
                    if key == "inputs" { "input" } else { "output" },
                    parent,
                )
            });
            scan_node(ex, src, value, Some(id), job, false);
        }
    }
}

fn step_list(
    ex: &mut Extracted,
    src: &str,
    steps: Node<'_>,
    parent: Option<usize>,
    job: Option<&str>,
) {
    for (ordinal, step) in yaml::items(steps).into_iter().enumerate() {
        let id = yaml::get(step, src, "id")
            .and_then(|n| yaml::literal(n, src))
            .unwrap_or_else(|| format!("#{}", ordinal + 1));
        let name = step_name(job, &id);
        let index = yaml::symbol(ex, src, step, name, "step", parent);
        link(ex, parent, Target::Symbol(index));
        if let Some(uses) = yaml::get(step, src, "uses") {
            uses_link(ex, src, uses, index, false);
        }
        scan_mapping(ex, src, step, Some(index), job, &["uses"]);
    }
}

fn step_name(job: Option<&str>, id: &str) -> String {
    match job {
        Some(job) => format!("step: {job}.{id}"),
        None => format!("step: {id}"),
    }
}

fn uses_link(ex: &mut Extracted, src: &str, node: Node<'_>, owner: usize, workflow: bool) {
    let Some(spec) = yaml::literal(node, src) else {
        return;
    };
    // Remote refs retain owner/repo/path@version, including docker:// images.
    if !spec.starts_with("./") {
        if !(spec.starts_with("docker://") || spec.contains('@')) {
            return;
        }
        ex.imports.push(spec.clone());
    }
    link(ex, Some(owner), Target::Action { spec, workflow });
}

fn scan_mapping(
    ex: &mut Extracted,
    src: &str,
    node: Node<'_>,
    owner: Option<usize>,
    job: Option<&str>,
    skip: &[&str],
) {
    for (key, value) in yaml::pairs(node, src) {
        if !skip.contains(&key.as_str()) {
            scan_node(ex, src, value, owner, job, key == "if");
        }
    }
}

fn scan_node(
    ex: &mut Extracted,
    src: &str,
    node: Node<'_>,
    owner: Option<usize>,
    job: Option<&str>,
    implicit: bool,
) {
    let node = yaml::unwrap(node);
    if matches!(
        node.kind(),
        "plain_scalar" | "single_quote_scalar" | "double_quote_scalar" | "block_scalar"
    ) {
        let text = yaml::scalar(node, src).unwrap_or_else(|| src[node.byte_range()].to_string());
        let mut expressions = Vec::new();
        if implicit && !text.contains("${{") {
            expressions.push(text.as_str());
        } else {
            let mut rest = text.as_str();
            while let Some((_, after)) = rest.split_once("${{") {
                let Some(end) = expression_end(after) else {
                    break;
                };
                expressions.push(&after[..end]);
                rest = &after[end + 2..];
            }
        }
        for expression in expressions {
            for parts in references(expression) {
                let target = match parts.as_slice() {
                    [context, id, rest @ ..] if context == "needs" || context == "jobs" => {
                        if let [outputs, output, ..] = rest
                            && outputs == "outputs"
                        {
                            Some(format!("output: job.{id}.{output}"))
                        } else {
                            Some(format!("job: {id}"))
                        }
                    }
                    [context, id, ..] if context == "steps" => Some(step_name(job, id)),
                    [context, id, ..] if context == "inputs" => Some(format!("input.{id}")),
                    _ => None,
                };
                if let Some(name) = target {
                    link(ex, owner, Target::Local(name));
                }
            }
        }
    } else if matches!(node.kind(), "block_mapping" | "flow_mapping") {
        for (_, value) in yaml::pairs(node, src) {
            scan_node(ex, src, value, owner, job, false);
        }
    } else {
        for child in yaml::items(node) {
            scan_node(ex, src, child, owner, job, false);
        }
    }
}

fn expression_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            quoted(bytes, &mut i)?;
        } else if bytes[i..].starts_with(b"}}") {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

fn whitespace(bytes: &[u8], i: &mut usize) {
    while bytes.get(*i).is_some_and(u8::is_ascii_whitespace) {
        *i += 1;
    }
}
fn ident(bytes: &[u8], i: &mut usize) -> Option<String> {
    let start = *i;
    if !bytes
        .get(*i)
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
    {
        return None;
    }
    *i += 1;
    while bytes
        .get(*i)
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
        *i += 1;
    }
    String::from_utf8(bytes[start..*i].to_vec()).ok()
}
fn quoted(bytes: &[u8], i: &mut usize) -> Option<String> {
    if bytes.get(*i) != Some(&b'\'') {
        return None;
    }
    *i += 1;
    let mut value = Vec::new();
    while let Some(&b) = bytes.get(*i) {
        *i += 1;
        if b == b'\'' {
            if bytes.get(*i) == Some(&b'\'') {
                value.push(b'\'');
                *i += 1;
            } else {
                return String::from_utf8(value).ok();
            }
        } else {
            value.push(b);
        }
    }
    None
}

/// Tokenize context paths while skipping quoted strings and dynamic subscripts.
/// A regex over the full YAML would invent edges from comments and string values.
fn references(text: &str) -> Vec<Vec<String>> {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut result = Vec::new();
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if quoted(bytes, &mut i).is_none() {
                break;
            }
            continue;
        }
        let Some(root) = ident(bytes, &mut i) else {
            i += 1;
            continue;
        };
        let mut parts = vec![root];
        loop {
            whitespace(bytes, &mut i);
            match bytes.get(i) {
                Some(b'.') => {
                    i += 1;
                    whitespace(bytes, &mut i);
                    let Some(key) = ident(bytes, &mut i) else {
                        break;
                    };
                    parts.push(key);
                }
                Some(b'[') => {
                    i += 1;
                    whitespace(bytes, &mut i);
                    let Some(key) = quoted(bytes, &mut i) else {
                        while i < bytes.len() && bytes[i] != b']' {
                            i += 1;
                        }
                        if i < bytes.len() {
                            i += 1;
                        }
                        // The indexed component is unknown. Do not resolve the
                        // root to a similarly named definition elsewhere.
                        parts.clear();
                        break;
                    };
                    whitespace(bytes, &mut i);
                    if bytes.get(i) != Some(&b']') {
                        parts.clear();
                        break;
                    }
                    i += 1;
                    parts.push(key);
                }
                _ => break,
            }
        }
        if parts.len() > 1 {
            result.push(parts);
        }
    }
    result
}

pub fn resolve(
    pending: &[Pending],
    files: &HashMap<String, i64>,
    external: &HashMap<String, i64>,
) -> Resolved {
    let mut result = Resolved {
        edges: Vec::new(),
        unresolved: 0,
    };
    for file in pending
        .iter()
        .filter(|f| f.extracted.automation.dialect == yaml::Dialect::GitHubActions)
    {
        let mut names: HashMap<&str, Vec<i64>> = HashMap::new();
        for (i, s) in file.extracted.symbols.iter().enumerate() {
            names.entry(&s.name).or_default().push(file.symbol_ids[i]);
        }
        for link in &file.extracted.automation.links {
            let from = link
                .from
                .map(|i| file.symbol_ids[i])
                .unwrap_or(file.file_symbol);
            let (target, kind) = match &link.target {
                Target::Symbol(i) => (Some(file.symbol_ids[*i]), "calls"),
                Target::Local(name) => (
                    names
                        .get(name.as_str())
                        .and_then(|ids| {
                            if let [id] = ids.as_slice() {
                                Some(*id)
                            } else {
                                None
                            }
                        })
                        .or_else(|| {
                            // Reusable jobs expose the called workflow's outputs;
                            // they do not declare a jobs.<id>.outputs mapping here.
                            let (job, _) = name.strip_prefix("output: job.")?.split_once('.')?;
                            let [id] = names.get(format!("job: {job}").as_str())?.as_slice() else {
                                return None;
                            };
                            file.extracted
                                .automation
                                .links
                                .iter()
                                .any(|link| {
                                    link.from.is_some_and(|i| file.symbol_ids[i] == *id)
                                        && matches!(
                                            link.target,
                                            Target::Action { workflow: true, .. }
                                        )
                                })
                                .then_some(*id)
                        }),
                    "calls",
                ),
                Target::Action { spec, workflow } => {
                    let target = if spec.starts_with("./") {
                        yaml::path("", spec).and_then(|path| {
                            if *workflow {
                                if Path::new(&path).parent() == Some(Path::new(".github/workflows"))
                                    && matches!(
                                        Path::new(&path).extension().and_then(|e| e.to_str()),
                                        Some("yml" | "yaml")
                                    )
                                {
                                    files.get(&path).copied()
                                } else {
                                    None
                                }
                            } else {
                                [
                                    yaml::path(&path, "action.yml").unwrap(),
                                    yaml::path(&path, "action.yaml").unwrap(),
                                ]
                                .iter()
                                .find_map(|p| files.get(p).copied())
                            }
                        })
                    } else {
                        external.get(spec).copied()
                    };
                    (target, "imports")
                }
                Target::File(candidates) => (
                    candidates.iter().find_map(|p| files.get(p).copied()),
                    "imports",
                ),
                _ => continue,
            };
            if let Some(target) = target {
                if from != target {
                    result.edges.push((from, target, kind));
                }
                if kind == "imports" {
                    result.edges.push((file.file_symbol, target, kind));
                }
            } else {
                result.unresolved += 1;
            }
        }
    }
    result.edges.sort_unstable();
    result.edges.dedup();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expression_paths_support_static_subscripts_without_reading_string_literals() {
        assert_eq!(
            references("steps['build-tool'].outputs['version']"),
            vec![vec!["steps", "build-tool", "outputs", "version"]]
        );
        assert!(references("format('steps.phantom.outputs.x needs.ghost.result')").is_empty());
        assert!(
            references("steps[matrix.target].outputs.x")
                .iter()
                .all(|p| p[0] != "steps")
        );
        assert_eq!(
            references("needs.build.result == 'success' && steps.test.outcome == 'success'").len(),
            2
        );
        let expression = "format('}} needs.fake.result', steps.real.outputs.x) }} suffix";
        let end = expression_end(expression).unwrap();
        assert_eq!(
            references(&expression[..end]),
            vec![vec!["steps", "real", "outputs", "x"]]
        );
    }
}
