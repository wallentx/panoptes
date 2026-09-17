//! Shared, conservative YAML syntax helpers and cached automation relationships.
//! No YAML constructors, expressions, or task code are executed.
use crate::extract::{Extracted, Symbol};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};
use tree_sitter::Node;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Automation {
    #[serde(default)]
    pub github_actions: bool,
    pub links: Vec<Link>,
    pub handlers: Vec<Handler>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub from: Option<usize>,
    /// Ansible play symbol, or file scope for standalone task/role files.
    pub scope: Option<usize>,
    pub target: Target,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Target {
    /// A uniquely named definition in this file only.
    Local(String),
    Action {
        spec: String,
        workflow: bool,
    },
    /// A definition in this extraction payload (execution/containment dependency).
    Symbol(usize),
    /// Ordered alternatives; the first existing indexed file wins.
    File(Vec<String>),
    Role {
        bases: Vec<String>,
        entries: Vec<String>,
    },
    Notify(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handler {
    pub symbol: usize,
    pub scope: Option<usize>,
    pub name: String,
    pub listen: Vec<String>,
}

pub fn children(node: Node<'_>) -> impl Iterator<Item = Node<'_>> {
    (0..node.named_child_count()).filter_map(move |i| node.named_child(i as u32))
}

pub fn unwrap(mut node: Node<'_>) -> Node<'_> {
    while matches!(
        node.kind(),
        "document" | "block_node" | "flow_node" | "block_sequence_item"
    ) {
        let Some(child) = children(node).find(|n| {
            !matches!(
                n.kind(),
                "anchor" | "tag" | "comment" | "yaml_directive" | "tag_directive"
            )
        }) else {
            break;
        };
        node = child;
    }
    node
}

pub fn pairs<'a>(node: Node<'a>, src: &str) -> Vec<(String, Node<'a>)> {
    children(unwrap(node))
        .filter_map(|pair| {
            if !matches!(pair.kind(), "block_mapping_pair" | "flow_pair") {
                return None;
            }
            Some((
                scalar(pair.child_by_field_name("key")?, src)?,
                pair.child_by_field_name("value")?,
            ))
        })
        .collect()
}

pub fn get<'a>(node: Node<'a>, src: &str, key: &str) -> Option<Node<'a>> {
    pairs(node, src)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

pub fn items(node: Node<'_>) -> Vec<Node<'_>> {
    let node = unwrap(node);
    if matches!(node.kind(), "block_sequence" | "flow_sequence") {
        children(node)
            .filter(|n| n.kind() != "comment")
            .map(unwrap)
            .collect()
    } else {
        Vec::new()
    }
}

pub fn scalar(node: Node<'_>, src: &str) -> Option<String> {
    let node = unwrap(node);
    let raw = src[node.byte_range()].trim();
    match node.kind() {
        "plain_scalar" => Some(raw.to_string()),
        "single_quote_scalar" => Some(
            raw.strip_prefix('\'')?
                .strip_suffix('\'')?
                .replace("''", "'"),
        ),
        // JSON is a safe subset of YAML double-quoted strings. Unsupported YAML
        // escapes stay unresolved instead of being interpreted as another name.
        "double_quote_scalar" => serde_json::from_str(raw).ok(),
        _ => None,
    }
}

pub fn literal(node: Node<'_>, src: &str) -> Option<String> {
    scalar(node, src)
        .filter(|s| !s.is_empty() && !s.contains("{{") && !s.contains("{%") && !s.contains("${{"))
}

pub fn strings(node: Node<'_>, src: &str) -> Vec<String> {
    if let Some(s) = literal(node, src) {
        vec![s]
    } else {
        items(node)
            .into_iter()
            .filter_map(|n| literal(n, src))
            .collect()
    }
}

/// Reject paths escaping the repository; do not turn ../../x into x.
pub fn path(base: &str, relative: &str) -> Option<String> {
    if Path::new(relative).is_absolute() {
        return None;
    }
    let joined = Path::new(base).join(relative);
    let mut parts = Vec::new();
    for part in joined.components() {
        match part {
            Component::Normal(p) => parts.push(p.to_str()?),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

pub fn yaml_candidates(path: String) -> Vec<String> {
    if Path::new(&path).extension().is_some() {
        vec![path]
    } else {
        vec![format!("{path}.yml"), format!("{path}.yaml")]
    }
}

pub fn symbol(
    ex: &mut Extracted,
    src: &str,
    node: Node<'_>,
    name: String,
    kind: &str,
    parent: Option<usize>,
) -> usize {
    let index = ex.symbols.len();
    let text = &src[node.byte_range()];
    let mut end = text.len().min(32 * 1024);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    ex.symbols.push(Symbol {
        name,
        kind: kind.to_string(),
        start_line: node.start_position().row as i64 + 1,
        end_line: node.end_position().row as i64 + 1,
        signature: text
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .chars()
            .take(200)
            .collect(),
        search_text: text[..end].to_string(),
    });
    ex.parents.push(parent);
    ex.containers.push(None);
    index
}
