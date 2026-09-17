//! Shared, conservative YAML syntax helpers and cached automation relationships.
//! No YAML constructors, expressions, or task code are executed.
use crate::extract::{Extracted, Symbol};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};
use tree_sitter::Node;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Automation {
    #[serde(default)]
    pub dialect: Dialect,
    pub links: Vec<Link>,
    pub handlers: Vec<Handler>,
    #[serde(default)]
    pub resources: Vec<crate::kubernetes::Resource>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dialect {
    #[default]
    Generic,
    GitHubActions,
    Compose,
    Kubernetes,
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
    Kube(crate::kubernetes::Key),
    SelectPods {
        namespace: String,
        labels: Vec<(String, String)>,
    },
    Named {
        path: String,
        name: String,
    },
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

/// Per-parse alias bindings and memoized effective mappings. Nodes keep their
/// original source spans; inheritance never manufactures a rewritten source file.
pub struct Yaml<'tree, 'src> {
    pub text: &'src str,
    pub aliases: Vec<(Node<'tree>, Node<'tree>)>,
    targets: HashMap<usize, Node<'tree>>,
    mappings: RefCell<HashMap<usize, Vec<(String, Node<'tree>)>>>,
}

impl std::ops::Deref for Yaml<'_, '_> {
    type Target = str;
    fn deref(&self) -> &str {
        self.text
    }
}

impl<'tree, 'src> Yaml<'tree, 'src> {
    pub fn new(root: Node<'tree>, text: &'src str) -> Self {
        let mut aliases = Vec::new();
        let mut targets = HashMap::new();
        for doc in children(root).filter(|n| n.kind() == "document") {
            let mut anchors = HashMap::<&str, Node<'tree>>::new();
            let mut todo = vec![doc];
            while let Some(node) = todo.pop() {
                if let Some(name) = node.named_child(0) {
                    let name = &text[name.byte_range()];
                    if node.kind() == "anchor" {
                        anchors.insert(name, node);
                    } else if node.kind() == "alias"
                        && let Some(&anchor) = anchors.get(name)
                        && let Some(parent) = anchor.parent()
                    {
                        let value = unwrap(parent);
                        // Recursive aliases have no finite expanded value.
                        if !(value.start_byte() <= node.start_byte()
                            && node.end_byte() <= value.end_byte())
                        {
                            aliases.push((node, anchor));
                            targets.insert(node.id(), value);
                        }
                    }
                }
                let mut nested: Vec<_> = children(node).collect();
                nested.reverse();
                todo.extend(nested);
            }
        }
        Self {
            text,
            aliases,
            targets,
            mappings: RefCell::new(HashMap::new()),
        }
    }
}

pub fn resolve<'a>(node: Node<'a>, src: &Yaml<'a, '_>) -> Node<'a> {
    let mut current = unwrap(node);
    for _ in 0..64 {
        let Some(&target) = src.targets.get(&current.id()) else {
            return current;
        };
        current = unwrap(target);
    }
    // Leave excessively deep chains unresolved.
    unwrap(node)
}

fn raw_pairs<'a>(node: Node<'a>, src: &Yaml<'a, '_>) -> Vec<(String, Node<'a>, bool)> {
    children(node)
        .filter_map(|pair| {
            if !matches!(pair.kind(), "block_mapping_pair" | "flow_pair") {
                return None;
            }
            let key = pair.child_by_field_name("key")?;
            let name = scalar(key, src)?;
            let merge = name == "<<" && unwrap(key).kind() == "plain_scalar";
            Some((
                name,
                pair.child_by_field_name("value").unwrap_or(pair),
                merge,
            ))
        })
        .collect()
}

pub fn pairs<'a>(node: Node<'a>, src: &Yaml<'a, '_>) -> Vec<(String, Node<'a>)> {
    fn effective<'a>(
        node: Node<'a>,
        src: &Yaml<'a, '_>,
        active: &mut HashSet<usize>,
    ) -> Vec<(String, Node<'a>)> {
        let node = resolve(node, src);
        if let Some(cached) = src.mappings.borrow().get(&node.id()) {
            return cached.clone();
        }
        if active.len() >= 64 || !active.insert(node.id()) {
            return Vec::new();
        }
        let raw = raw_pairs(node, src);
        let mut result: Vec<_> = raw
            .iter()
            .filter(|(_, _, merge)| !merge)
            .map(|(k, v, _)| (k.clone(), *v))
            .collect();
        let mut seen: HashSet<_> = result.iter().map(|(k, _)| k.clone()).collect();
        for (_, value, _) in raw.iter().filter(|(_, _, merge)| *merge) {
            let value = resolve(*value, src);
            let sources = if matches!(value.kind(), "block_sequence" | "flow_sequence") {
                items(value, src)
            } else {
                vec![value]
            };
            for source in sources {
                for (key, value) in effective(source, src, active) {
                    // Explicit values win; earlier maps in a merge sequence win.
                    if seen.insert(key.clone()) {
                        result.push((key, value));
                    }
                }
            }
        }
        active.remove(&node.id());
        src.mappings.borrow_mut().insert(node.id(), result.clone());
        result
    }
    effective(node, src, &mut HashSet::new())
}

pub fn get<'a>(node: Node<'a>, src: &Yaml<'a, '_>, key: &str) -> Option<Node<'a>> {
    let matches: Vec<_> = pairs(node, src)
        .into_iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v)
        .collect();
    if let [only] = matches.as_slice() {
        Some(*only)
    } else {
        None
    }
}

pub fn items<'a>(node: Node<'a>, src: &Yaml<'a, '_>) -> Vec<Node<'a>> {
    let node = resolve(node, src);
    if matches!(node.kind(), "block_sequence" | "flow_sequence") {
        // Keep an alias item's own span, resolving it only when inspecting its
        // fields. This attributes inherited dependencies to their consumer.
        children(node)
            .filter(|n| n.kind() != "comment")
            .map(unwrap)
            .collect()
    } else {
        Vec::new()
    }
}

pub fn scalar<'a>(node: Node<'a>, src: &Yaml<'a, '_>) -> Option<String> {
    let node = resolve(node, src);
    let raw = src[node.byte_range()].trim();
    match node.kind() {
        "plain_scalar" => Some(raw.to_string()),
        "single_quote_scalar" => Some(
            raw.strip_prefix('\'')?
                .strip_suffix('\'')?
                .replace("''", "'"),
        ),
        "double_quote_scalar" => serde_json::from_str(raw).ok(),
        _ => None,
    }
}

pub fn literal<'a>(node: Node<'a>, src: &Yaml<'a, '_>) -> Option<String> {
    scalar(node, src)
        .filter(|s| !s.is_empty() && !s.contains("{{") && !s.contains("{%") && !s.contains("${{"))
}

pub fn strings<'a>(node: Node<'a>, src: &Yaml<'a, '_>) -> Vec<String> {
    if let Some(s) = literal(node, src) {
        vec![s]
    } else {
        items(node, src)
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

#[cfg(test)]
mod tests {
    use super::*;
    fn check(text: &str, test: impl for<'tree> FnOnce(Node<'tree>, &Yaml<'tree, '_>)) {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_yaml::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(text, None).unwrap();
        let root = tree.root_node();
        let yaml = Yaml::new(root, text);
        test(root, &yaml);
    }

    #[test]
    fn merge_precedence_null_overrides_and_source_spans() {
        check(
            "a: &a {value: first, inherited: yes}\nb: &b {value: second, other: yes}\nc: {<<: [*a, *b]}\nd: {value: explicit, <<: [*a, *b], inherited: null}\ne: {'<<': *a}\n",
            |root, yaml| {
                let doc = root.named_child(0).unwrap();
                let c = get(doc, yaml, "c").unwrap();
                let value = get(c, yaml, "value").unwrap();
                assert_eq!(literal(value, yaml).as_deref(), Some("first"));
                assert_eq!(
                    value.start_position().row,
                    0,
                    "inherited value keeps anchor source span"
                );
                let d = get(doc, yaml, "d").unwrap();
                assert_eq!(
                    literal(get(d, yaml, "value").unwrap(), yaml).as_deref(),
                    Some("explicit")
                );
                assert_eq!(
                    literal(get(d, yaml, "inherited").unwrap(), yaml).as_deref(),
                    Some("null")
                );
                assert!(get(get(doc, yaml, "e").unwrap(), yaml, "value").is_none());
            },
        );
    }

    #[test]
    fn anchors_are_ordered_document_local_and_recursive_aliases_terminate() {
        check(
            "a: &same one\nb: *same\nc: &same two\nd: *same\ncycle: &cycle {<<: *cycle, value: ok}\nforward: *later\nlater: &later three\n---\ne: *same\n",
            |root, yaml| {
                let docs: Vec<_> = children(root).filter(|n| n.kind() == "document").collect();
                assert_eq!(
                    literal(get(docs[0], yaml, "b").unwrap(), yaml).as_deref(),
                    Some("one")
                );
                assert_eq!(
                    literal(get(docs[0], yaml, "d").unwrap(), yaml).as_deref(),
                    Some("two")
                );
                assert!(literal(get(docs[1], yaml, "e").unwrap(), yaml).is_none());
                assert!(literal(get(docs[0], yaml, "forward").unwrap(), yaml).is_none());
                assert_eq!(pairs(get(docs[0], yaml, "cycle").unwrap(), yaml).len(), 1);
                assert_eq!(yaml.aliases.len(), 2);
            },
        );
    }

    #[test]
    fn repeated_merge_graphs_do_not_expand_exponentially() {
        let mut text = "v0: &v0 {name: stable}\n".to_string();
        for n in 1..40 {
            text.push_str(&format!("v{n}: &v{n} {{<<: [*v{}, *v{}]}}\n", n - 1, n - 1));
        }
        check(&text, |root, yaml| {
            let doc = root.named_child(0).unwrap();
            let last = get(doc, yaml, "v39").unwrap();
            assert_eq!(pairs(last, yaml).len(), 1);
            assert!(yaml.mappings.borrow().len() <= 41);
        });
    }
}
