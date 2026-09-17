//! Static Ansible structure and repository-local dependency resolution.
use crate::extract::Extracted;
use crate::index::Pending;
use crate::yaml::{self, Handler, Link, Target};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tree_sitter::Node;

fn parent(path: &str) -> &str {
    Path::new(path)
        .parent()
        .and_then(Path::to_str)
        .unwrap_or("")
}

/// Conventional role layout, including a standalone role repository.
fn role_root(path: &str) -> Option<String> {
    let parts: Vec<_> = path.split('/').collect();
    for i in (0..parts.len().saturating_sub(1)).rev() {
        if matches!(
            parts[i],
            "tasks" | "handlers" | "meta" | "vars" | "defaults"
        ) && (i == 0 || (i >= 2 && parts[i - 2] == "roles"))
        {
            return Some(parts[..i].join("/"));
        }
    }
    None
}

fn module_key(key: &str) -> &str {
    key.strip_prefix("ansible.builtin.")
        .or_else(|| key.strip_prefix("ansible.legacy."))
        .unwrap_or(key)
}

fn task_like<'tree>(node: Node<'tree>, src: &yaml::Yaml<'tree, '_>) -> bool {
    yaml::pairs(node, src).iter().any(|(key, _)| {
        key.split('.').count() == 3
            || key.starts_with("ansible.builtin.")
            || matches!(
                module_key(key),
                "include_tasks"
                    | "import_tasks"
                    | "include_role"
                    | "import_role"
                    | "include_vars"
                    | "block"
                    | "debug"
                    | "set_fact"
                    | "copy"
                    | "template"
                    | "command"
                    | "shell"
                    | "service"
                    | "systemd"
                    | "package"
                    | "apt"
                    | "yum"
                    | "file"
                    | "stat"
                    | "assert"
                    | "fail"
                    | "uri"
                    | "get_url"
            )
    })
}

pub fn enrich<'tree>(
    root: Node<'tree>,
    src: &yaml::Yaml<'tree, '_>,
    path: &str,
    ex: &mut Extracted,
) {
    if path.starts_with(".github/") {
        return;
    }
    for doc in yaml::children(root).filter(|n| n.kind() == "document") {
        let top = yaml::resolve(doc, src);
        let entries = yaml::items(top, src);
        let is_playbook = entries.iter().any(|&node| {
            yaml::get(node, src, "hosts").is_some()
                || yaml::pairs(node, src)
                    .iter()
                    .any(|(k, _)| module_key(k) == "import_playbook")
        });
        if is_playbook {
            for play in entries {
                if let Some((_, value)) = yaml::pairs(play, src)
                    .into_iter()
                    .find(|(k, _)| module_key(k) == "import_playbook")
                {
                    file_link(ex, src, path, value, None, None, "playbook");
                    continue;
                }
                let name = yaml::get(play, src, "name")
                    .and_then(|n| yaml::scalar(n, src))
                    .or_else(|| yaml::get(play, src, "hosts").and_then(|n| yaml::scalar(n, src)))
                    .unwrap_or_else(|| format!("line {}", play.start_position().row + 1));
                let id = yaml::symbol(ex, src, play, format!("play: {name}"), "play", None);
                ex.automation.links.push(Link {
                    from: None,
                    scope: None,
                    target: Target::Symbol(id),
                });
                for key in ["pre_tasks", "tasks", "post_tasks", "handlers"] {
                    if let Some(tasks) = yaml::get(play, src, key) {
                        task_list(ex, src, path, tasks, Some(id), Some(id), key == "handlers");
                    }
                }
                if let Some(roles) = yaml::get(play, src, "roles") {
                    for role in yaml::items(roles, src) {
                        role_link(ex, src, path, role, Some(id), Some(id));
                    }
                }
                if let Some(vars) = yaml::get(play, src, "vars_files") {
                    for entry in yaml::items(vars, src) {
                        file_link(ex, src, path, entry, Some(id), Some(id), "vars");
                    }
                }
            }
        } else if role_root(path).is_some()
            || parent(path)
                .split('/')
                .any(|p| p == "tasks" || p == "handlers")
            || entries.iter().any(|&n| task_like(n, src))
        {
            if parent(path).ends_with("meta") {
                if let Some(dependencies) = yaml::get(top, src, "dependencies") {
                    for role in yaml::items(dependencies, src) {
                        role_link(ex, src, path, role, None, None);
                    }
                }
            } else {
                let handlers = path.split('/').any(|p| p == "handlers");
                task_list(ex, src, path, top, None, None, handlers);
            }
        }
    }
}

fn task_list<'tree>(
    ex: &mut Extracted,
    src: &yaml::Yaml<'tree, '_>,
    path: &str,
    list: Node<'tree>,
    parent: Option<usize>,
    scope: Option<usize>,
    handler: bool,
) {
    for task in yaml::items(list, src) {
        let pairs = yaml::pairs(task, src);
        if pairs.is_empty() {
            continue;
        }
        let name = yaml::get(task, src, "name")
            .and_then(|n| yaml::scalar(n, src))
            .unwrap_or_else(|| format!("line {}", task.start_position().row + 1));
        let kind = if handler { "handler" } else { "task" };
        let id = yaml::symbol(ex, src, task, format!("{kind}: {name}"), kind, parent);
        if !handler {
            ex.automation.links.push(Link {
                from: parent,
                scope,
                target: Target::Symbol(id),
            });
        }
        if handler {
            let listen = yaml::get(task, src, "listen")
                .map(|n| yaml::strings(n, src))
                .unwrap_or_default();
            ex.automation.handlers.push(Handler {
                symbol: id,
                scope,
                name,
                listen,
            });
        }
        for (key, value) in pairs {
            match module_key(&key) {
                "block" | "rescue" | "always" => {
                    task_list(ex, src, path, value, Some(id), scope, handler)
                }
                "include_tasks" | "import_tasks" => {
                    file_link(ex, src, path, value, Some(id), scope, "tasks")
                }
                "include_vars" => file_link(ex, src, path, value, Some(id), scope, "vars"),
                "include_role" | "import_role" => role_link(ex, src, path, value, Some(id), scope),
                "notify" => {
                    for name in yaml::strings(value, src) {
                        ex.automation.links.push(Link {
                            from: Some(id),
                            scope,
                            target: Target::Notify(name),
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

fn file_link<'tree>(
    ex: &mut Extracted,
    src: &yaml::Yaml<'tree, '_>,
    path: &str,
    value: Node<'tree>,
    from: Option<usize>,
    scope: Option<usize>,
    kind: &str,
) {
    let value = yaml::get(value, src, "file").unwrap_or(value);
    let items = yaml::items(value, src);
    if items.iter().any(|&n| yaml::literal(n, src).is_none()) {
        return;
    }
    let specs = yaml::strings(value, src);
    let mut candidates = Vec::new();
    for spec in specs {
        // Role task/vars lookup starts in the role's corresponding directory.
        if kind != "playbook"
            && let Some(role) = role_root(path)
            && let Some(p) = yaml::path(&role, &format!("{kind}/{spec}"))
        {
            candidates.push(p);
        }
        if let Some(p) = yaml::path(parent(path), &spec) {
            candidates.push(p);
        }
    }
    if !candidates.is_empty() {
        ex.automation.links.push(Link {
            from,
            scope,
            target: Target::File(candidates),
        });
    }
}

fn role_link<'tree>(
    ex: &mut Extracted,
    src: &yaml::Yaml<'tree, '_>,
    path: &str,
    value: Node<'tree>,
    from: Option<usize>,
    scope: Option<usize>,
) {
    let name = yaml::get(value, src, "role")
        .or_else(|| yaml::get(value, src, "name"))
        .unwrap_or(value);
    let Some(name) = yaml::literal(name, src) else {
        return;
    };
    if Path::new(&name).is_absolute() {
        return;
    }
    let mut bases = Vec::new();
    if let Some(role) = role_root(path)
        && let Some(p) = yaml::path(parent(&role), &name)
    {
        bases.push(p);
    }
    for base in [parent(path), ""] {
        if let Some(p) = yaml::path(base, &format!("roles/{name}")) {
            bases.push(p);
        }
    }
    if let Some(p) = yaml::path(parent(path), &name) {
        bases.push(p);
    }
    // Fully qualified collection roles stored inside the indexed repository.
    let parts: Vec<_> = name.split('.').collect();
    if let [namespace, collection, role] = parts.as_slice() {
        for base in ["collections/ansible_collections", "ansible_collections"] {
            bases.insert(0, format!("{base}/{namespace}/{collection}/roles/{role}"));
        }
    }
    let mut entries = Vec::new();
    for (dir, option) in [
        ("tasks", "tasks_from"),
        ("handlers", "handlers_from"),
        ("vars", "vars_from"),
        ("defaults", "defaults_from"),
        ("meta", ""),
    ] {
        let stem = match yaml::get(value, src, option) {
            Some(n) => yaml::literal(n, src),
            None => Some("main".to_string()),
        };
        if let Some(stem) = stem
            && let Some(p) = yaml::path(dir, &stem)
        {
            entries.push(p);
        }
    }
    ex.automation.links.push(Link {
        from,
        scope,
        target: Target::Role { bases, entries },
    });
}

pub struct Resolved {
    pub edges: Vec<(i64, i64, &'static str)>,
    pub unresolved: usize,
}

pub fn resolve(pending: &[Pending], files: &HashMap<String, i64>) -> Resolved {
    let mut result = Resolved {
        edges: Vec::new(),
        unresolved: 0,
    };
    // Visibility is scoped to a play, not to every play in the same YAML file.
    let mut imports: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut handlers: HashMap<i64, Vec<(&Pending, &Handler)>> = HashMap::new();
    let mut notifications = Vec::new();
    let mut plays = Vec::new();
    for file in pending {
        if file.extracted.automation.dialect != yaml::Dialect::Generic {
            continue;
        }
        let id = |index: Option<usize>| {
            index
                .map(|i| file.symbol_ids[i])
                .unwrap_or(file.file_symbol)
        };
        for (index, symbol) in file.extracted.symbols.iter().enumerate() {
            if symbol.kind == "play" {
                plays.push(file.symbol_ids[index]);
            }
        }
        for handler in &file.extracted.automation.handlers {
            handlers
                .entry(id(handler.scope))
                .or_default()
                .push((file, handler));
        }
        for link in &file.extracted.automation.links {
            let from = id(link.from);
            let scope = id(link.scope);
            let targets = match &link.target {
                Target::External(_)
                | Target::KustomizeResource(_)
                | Target::Patch { .. }
                | Target::Local(_)
                | Target::Named { .. }
                | Target::Action { .. }
                | Target::Kube(_)
                | Target::SelectPods { .. } => continue,
                Target::Symbol(index) => {
                    result.edges.push((from, file.symbol_ids[*index], "calls"));
                    continue;
                }
                Target::File(candidates) => candidates
                    .iter()
                    .filter(|p| {
                        matches!(
                            Path::new(p).extension().and_then(|e| e.to_str()),
                            Some("yml" | "yaml")
                        )
                    })
                    .find_map(|p| files.get(p).copied())
                    .into_iter()
                    .collect::<Vec<_>>(),
                Target::Role { bases, entries } => {
                    let mut targets = Vec::new();
                    for base in bases {
                        // Pick one role directory, even if its only indexed file
                        // is not one of the requested entry points.
                        let prefix = format!("{base}/");
                        if !files.keys().any(|p| p.starts_with(&prefix)) {
                            continue;
                        }
                        for entry in entries {
                            if let Some(path) = yaml::path(base, entry)
                                && let Some(id) = yaml::yaml_candidates(path)
                                    .iter()
                                    .find_map(|p| files.get(p))
                            {
                                targets.push(*id);
                            }
                        }
                        break;
                    }
                    targets
                }
                Target::Notify(name) => {
                    notifications.push((file, from, scope, name));
                    continue;
                }
            };
            if targets.is_empty() {
                result.unresolved += 1;
            }
            for target in targets {
                result.edges.push((from, target, "imports"));
                result.edges.push((file.file_symbol, target, "imports"));
                imports.entry(scope).or_default().push(target);
            }
        }
    }
    fn reachable(start: i64, imports: &HashMap<i64, Vec<i64>>) -> HashSet<i64> {
        let mut seen = HashSet::new();
        let mut todo = vec![start];
        while let Some(id) = todo.pop() {
            if seen.insert(id)
                && let Some(next) = imports.get(&id)
            {
                todo.extend(next);
            }
        }
        seen
    }
    let contexts: Vec<_> = plays.iter().map(|&id| reachable(id, &imports)).collect();
    for (file, from, scope, name) in notifications {
        let mut local = reachable(scope, &imports);
        // A standalone role can be indexed without any playbook using it.
        if let Some(role) = role_root(&file.rel) {
            for entry in ["handlers/main", "meta/main"] {
                if let Some(path) = yaml::path(&role, entry)
                    && let Some(id) = yaml::yaml_candidates(path)
                        .iter()
                        .find_map(|p| files.get(p))
                {
                    local.extend(reachable(*id, &imports));
                }
            }
        }
        let mut contexts_for_scope: Vec<_> =
            contexts.iter().filter(|set| set.contains(&scope)).collect();
        if contexts_for_scope.is_empty() {
            contexts_for_scope.push(&local);
        }
        let mut targets = HashSet::new();
        for context in contexts_for_scope {
            let mut named = HashSet::new();
            let mut listening = HashSet::new();
            for visible in context {
                if let Some(defs) = handlers.get(visible) {
                    for (file, handler) in defs {
                        let id = file.symbol_ids[handler.symbol];
                        let qualified = role_root(&file.rel).and_then(|p| {
                            Path::new(&p)
                                .file_name()
                                .map(|n| format!("{} : {}", n.to_string_lossy(), handler.name))
                        });
                        if handler.name == *name || qualified.as_deref() == Some(name.as_str()) {
                            named.insert(id);
                        }
                        if handler.listen.contains(name) {
                            listening.insert(id);
                        }
                    }
                }
            }
            // Ansible's last-loaded handler wins duplicate names. Without
            // execution order, keep that ambiguity unresolved; listen fans out.
            if named.len() == 1 {
                targets.extend(named);
            }
            targets.extend(listening);
        }
        if targets.is_empty() {
            result.unresolved += 1;
        }
        for target in targets {
            result.edges.push((from, target, "calls"));
        }
    }
    result.edges.sort_unstable();
    result.edges.dedup();
    result
}
