//! CloudFormation YAML declarations and intrinsic dependencies, without evaluation.
use crate::ansible::Resolved;
use crate::extract::Extracted;
use crate::index::Pending;
use crate::yaml::{self, Dialect, Link, Target, Yaml};
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;
fn link(ex: &mut Extracted, from: usize, scope: usize, target: Target) {
    ex.automation.links.push(Link {
        from: Some(from),
        scope: Some(scope),
        target,
    });
}
fn reference(ex: &mut Extracted, from: usize, scope: usize, name: &str, kinds: &[&str]) {
    if name.starts_with("AWS::") || name.is_empty() || name.contains(['$', '{', '}']) {
        return;
    }
    link(
        ex,
        from,
        scope,
        Target::OneOf(
            kinds
                .iter()
                .map(|kind| format!("cfn {kind}: {name}"))
                .collect(),
        ),
    );
}
fn tag<'a>(mut node: Node<'a>, yaml: &Yaml<'a, '_>) -> Option<String> {
    loop {
        if let Some(tag) = yaml::children(node).find(|n| n.kind() == "tag") {
            return Some(yaml[tag.byte_range()].to_string());
        }
        if !matches!(
            node.kind(),
            "block_node" | "flow_node" | "block_sequence_item"
        ) {
            return None;
        }
        node = yaml::children(node).find(|n| !matches!(n.kind(), "anchor" | "comment"))?;
    }
}
fn literal<'a>(node: Node<'a>, yaml: &Yaml<'a, '_>) -> Option<String> {
    if tag(node, yaml).is_some() {
        None
    } else {
        yaml::literal(node, yaml)
    }
}
fn sequence<'a>(node: Node<'a>, yaml: &Yaml<'a, '_>) -> Vec<Node<'a>> {
    let node = yaml::resolve(node, yaml);
    if matches!(node.kind(), "block_sequence" | "flow_sequence") {
        yaml::children(node)
            .filter(|n| n.kind() != "comment")
            .collect()
    } else {
        Vec::new()
    }
}

pub fn enrich<'a>(root: Node<'a>, yaml: &Yaml<'a, '_>, path: &str, ex: &mut Extracted) {
    if ex.automation.dialect != Dialect::Generic {
        return;
    }
    for (ordinal, doc) in yaml::children(root)
        .filter(|n| n.kind() == "document")
        .enumerate()
    {
        let resources = yaml::get(doc, yaml, "Resources");
        let recognized = yaml::get(doc, yaml, "AWSTemplateFormatVersion").is_some()
            || resources.is_some_and(|n| {
                yaml::pairs(n, yaml).iter().any(|(_, n)| {
                    yaml::get(*n, yaml, "Type")
                        .and_then(|n| literal(n, yaml))
                        .is_some_and(|s| s.contains("::"))
                })
            });
        if !recognized {
            continue;
        }
        ex.automation.dialect = Dialect::CloudFormation;
        let template = yaml::symbol(
            ex,
            yaml,
            doc,
            format!("cfn template: {path}#{}", ordinal + 1),
            "template",
            None,
        );
        ex.automation.links.push(Link {
            from: None,
            scope: Some(template),
            target: Target::Symbol(template),
        });
        for (section, kind) in [
            ("Parameters", "parameter"),
            ("Mappings", "mapping"),
            ("Conditions", "condition"),
            ("Resources", "resource"),
            ("Outputs", "output"),
        ] {
            if let Some(defs) = yaml::get(doc, yaml, section) {
                for (name, body) in yaml::pairs(defs, yaml) {
                    let id = yaml::symbol(
                        ex,
                        yaml,
                        body,
                        format!("cfn {kind}: {name}"),
                        kind,
                        Some(template),
                    );
                    if matches!(kind, "resource" | "output") {
                        link(ex, template, template, Target::Symbol(id));
                    }
                    if kind == "resource"
                        && let Some(depends) = yaml::get(body, yaml, "DependsOn")
                    {
                        let items = sequence(depends, yaml);
                        let values = if items.is_empty() {
                            vec![depends]
                        } else {
                            items
                        };
                        for value in values {
                            if let Some(name) = literal(value, yaml) {
                                reference(ex, id, template, &name, &["resource"]);
                            }
                        }
                    }
                    if matches!(kind, "resource" | "output")
                        && let Some(condition) =
                            yaml::get(body, yaml, "Condition").and_then(|n| literal(n, yaml))
                    {
                        reference(ex, id, template, &condition, &["condition"]);
                    }
                    if matches!(kind, "resource" | "output" | "condition") {
                        walk(ex, id, template, body, yaml, 0);
                    }
                }
            }
        }
    }
}

fn intrinsic<'a>(
    ex: &mut Extracted,
    id: usize,
    scope: usize,
    kind: &str,
    value: Node<'a>,
    yaml: &Yaml<'a, '_>,
    depth: usize,
) {
    match kind {
        "Ref" => {
            if let Some(name) = literal(value, yaml) {
                reference(ex, id, scope, &name, &["parameter", "resource"]);
            }
        }
        "GetAtt" => {
            let values = sequence(value, yaml);
            let name = if let Some(&first) = values.first() {
                literal(first, yaml)
            } else {
                literal(value, yaml)
                    .and_then(|s| s.split_once('.').map(|(name, _)| name.to_string()))
            };
            if let Some(name) = name {
                reference(ex, id, scope, &name, &["resource"]);
            }
        }
        "Condition" => {
            if let Some(name) = literal(value, yaml) {
                reference(ex, id, scope, &name, &["condition"]);
            }
        }
        "If" | "FindInMap" => {
            if let Some(&first) = sequence(value, yaml).first()
                && let Some(name) = literal(first, yaml)
            {
                reference(
                    ex,
                    id,
                    scope,
                    &name,
                    &[if kind == "If" { "condition" } else { "mapping" }],
                );
            }
        }
        "Sub" => {
            let values = sequence(value, yaml);
            let template = values.first().copied().unwrap_or(value);
            let shadowed: HashSet<_> = values
                .get(1)
                .map(|&n| yaml::pairs(n, yaml).into_iter().map(|(k, _)| k).collect())
                .unwrap_or_default();
            let raw = yaml::resolve(template, yaml);
            let text = literal(template, yaml).or_else(|| {
                (raw.kind() == "block_scalar").then(|| yaml[raw.byte_range()].to_string())
            });
            if let Some(text) = text {
                let mut rest = text.as_str();
                while let Some((_, after)) = rest.split_once("${") {
                    let Some((name, tail)) = after.split_once('}') else {
                        break;
                    };
                    rest = tail;
                    if name.starts_with('!') || shadowed.contains(name) {
                        continue;
                    }
                    if let Some((name, _)) = name.split_once('.') {
                        reference(ex, id, scope, name, &["resource"]);
                    } else {
                        reference(ex, id, scope, name, &["parameter", "resource"]);
                    }
                }
            }
            if let Some(&variables) = values.get(1) {
                walk(ex, id, scope, variables, yaml, depth + 1);
            }
            return;
        }
        _ => {}
    }
    // Nested intrinsics can contribute dependencies even when the outer result
    // (a dynamic logical ID, condition, or imported export name) is unknowable.
    walk(ex, id, scope, value, yaml, depth + 1);
}
fn walk<'a>(
    ex: &mut Extracted,
    id: usize,
    scope: usize,
    node: Node<'a>,
    yaml: &Yaml<'a, '_>,
    depth: usize,
) {
    if depth >= 128 {
        return;
    }
    if let Some(tag) = tag(node, yaml) {
        let value = yaml::resolve(node, yaml);
        intrinsic(
            ex,
            id,
            scope,
            tag.trim_start_matches('!'),
            value,
            yaml,
            depth + 1,
        );
        return;
    }
    let node = yaml::resolve(node, yaml);
    let pairs = yaml::pairs(node, yaml);
    if let [(key, value)] = pairs.as_slice()
        && (key == "Ref" || key == "Condition" || key.starts_with("Fn::"))
    {
        intrinsic(
            ex,
            id,
            scope,
            key.strip_prefix("Fn::").unwrap_or(key),
            *value,
            yaml,
            depth + 1,
        );
        return;
    }
    for (_, value) in pairs {
        walk(ex, id, scope, value, yaml, depth + 1);
    }
    for item in sequence(node, yaml) {
        walk(ex, id, scope, item, yaml, depth + 1);
    }
}

pub fn resolve(pending: &[Pending]) -> Resolved {
    let mut result = Resolved {
        edges: Vec::new(),
        unresolved: 0,
    };
    for file in pending
        .iter()
        .filter(|f| f.extracted.automation.dialect == Dialect::CloudFormation)
    {
        let mut names: HashMap<(Option<usize>, &str), Vec<i64>> = HashMap::new();
        for (i, symbol) in file.extracted.symbols.iter().enumerate() {
            names
                .entry((file.extracted.parents[i], &symbol.name))
                .or_default()
                .push(file.symbol_ids[i]);
        }
        for link in &file.extracted.automation.links {
            let from = link
                .from
                .map(|i| file.symbol_ids[i])
                .unwrap_or(file.file_symbol);
            let mut targets = match &link.target {
                Target::Symbol(i) => vec![file.symbol_ids[*i]],
                Target::OneOf(alternatives) => alternatives
                    .iter()
                    .flat_map(|name| {
                        names
                            .get(&(link.scope, name.as_str()))
                            .into_iter()
                            .flatten()
                            .copied()
                    })
                    .collect(),
                _ => continue,
            };
            targets.sort_unstable();
            targets.dedup();
            if let [target] = targets.as_slice() {
                result.edges.push((from, *target, "calls"));
            } else {
                result.unresolved += 1;
            }
        }
    }
    result.edges.sort_unstable();
    result.edges.dedup();
    result
}
