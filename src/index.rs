//! `build` and `grep`: the write and read ends of the store.

use anyhow::{Context, Result, anyhow};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

use crate::extract;
use crate::repo::{self, Lang, SourceFile};

/// Identifies the extractor. Any change to the queries or the stored shape must
/// bump this, because it is what tells an existing store its rows were produced
/// by a different extractor and cannot be trusted.
pub const EXTRACTOR_STAMP: &str = "panoptes-10";

pub struct BuildStats {
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
    pub unresolved: usize,
    pub parsed: usize,
    pub reused: usize,
    pub deleted: usize,
}

#[derive(Debug, Serialize)]
pub struct Freshness {
    pub indexed: bool,
    pub extractor_current: bool,
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl Freshness {
    pub fn is_clean(&self) -> bool {
        self.indexed
            && self.extractor_current
            && self.added.is_empty()
            && self.modified.is_empty()
            && self.deleted.is_empty()
    }

    pub fn changed_count(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }
}

/// Compare the live source set with stored hashes without mutating the store.
pub fn freshness(db: &Connection, root: &Path) -> Result<Freshness> {
    use rusqlite::OptionalExtension;

    let repo: Option<(i64, String)> = db
        .query_row(
            "select id, extractor_stamp from repos where root=?1",
            [root.to_string_lossy()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((repo_id, stamp)) = repo else {
        return Ok(Freshness {
            indexed: false,
            extractor_current: false,
            added: Vec::new(),
            modified: Vec::new(),
            deleted: Vec::new(),
        });
    };

    let files = repo::walk(root).context("walk the work tree for freshness")?;
    let live: HashMap<&str, &str> = files
        .iter()
        .map(|file| (file.rel.as_str(), file.hash.as_str()))
        .collect();
    let mut stored = HashMap::new();
    let mut statement = db.prepare("select path, hash from files where repo_id=?1")?;
    let rows = statement.query_map([repo_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (path, hash) = row?;
        stored.insert(path, hash);
    }

    let mut added = Vec::new();
    let mut modified = Vec::new();
    for (path, hash) in &live {
        match stored.get(*path) {
            None => added.push((*path).to_string()),
            Some(old) if old != hash => modified.push((*path).to_string()),
            Some(_) => {}
        }
    }
    let mut deleted: Vec<String> = stored
        .keys()
        .filter(|path| !live.contains_key(path.as_str()))
        .cloned()
        .collect();
    added.sort();
    modified.sort();
    deleted.sort();
    Ok(Freshness {
        indexed: true,
        extractor_current: stamp == EXTRACTOR_STAMP,
        added,
        modified,
        deleted,
    })
}

#[derive(Debug)]
struct ExistingFile {
    id: i64,
    hash: String,
    mtime: i64,
    size: i64,
}

pub(crate) struct Pending {
    pub(crate) rel: String,
    pub(crate) lang: Lang,
    pub(crate) file_id: i64,
    pub(crate) file_symbol: i64,
    pub(crate) symbol_ids: Vec<i64>,
    pub(crate) extracted: extract::Extracted,
}

/// Incrementally index `root` into `db`.
///
/// Unchanged files replay their serialized extraction payload and retain stable
/// file/symbol row ids. Changed and added files alone are parsed. Edges are then
/// rebuilt from all cached/current extraction payloads because a definition
/// change in one file can change resolution for callers in every other file.
pub fn build(db: &mut Connection, root: &Path) -> Result<BuildStats> {
    build_with_jobs(db, root, default_jobs())
}

/// Number of extraction workers used when a caller does not choose explicitly.
///
/// Four workers are enough to expose parser parallelism without letting a large
/// repository multiply its peak memory usage by every CPU visible to Android or
/// a shared CI runner. `PANOPTES_JOBS` is useful for repeatable benchmarks and for
/// low-memory devices; command-line builds expose the same value as `--jobs`.
pub fn default_jobs() -> usize {
    std::env::var("PANOPTES_JOBS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|jobs| *jobs > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
                .min(4)
        })
        .clamp(1, 32)
}

pub fn build_with_jobs(
    db: &mut Connection,
    root: &Path,
    requested_jobs: usize,
) -> Result<BuildStats> {
    let files = repo::walk(root).context("walk the work tree")?;
    let common = repo::git_common_dir(root);
    let now = unix_now();

    let tx = db.transaction().context("begin build transaction")?;
    tx.execute(
        "insert into repos(root, git_common_dir, indexed_at, extractor_stamp)
         values (?1, ?2, ?3, ?4)
         on conflict(root) do update set
           git_common_dir=excluded.git_common_dir,
           indexed_at=excluded.indexed_at,
           extractor_stamp=excluded.extractor_stamp",
        rusqlite::params![root.to_string_lossy(), common, now, EXTRACTOR_STAMP],
    )?;
    let repo_id: i64 = tx.query_row(
        "select id from repos where root=?1",
        [root.to_string_lossy()],
        |r| r.get(0),
    )?;

    let existing = load_existing_files(&tx, repo_id)?;
    let current_paths: std::collections::HashSet<&str> =
        files.iter().map(|f| f.rel.as_str()).collect();
    let deleted = existing
        .keys()
        .filter(|path| !current_paths.contains(path.as_str()))
        .count();

    // Every relationship is cheap to reconstruct from extraction payloads and
    // may depend on a changed definition elsewhere. Module nodes are derived from
    // unresolved imports and are recreated with those relationships.
    tx.execute("delete from edges where repo_id=?1", [repo_id])?;
    tx.execute(
        "delete from symbols where repo_id=?1 and kind='module'",
        [repo_id],
    )?;
    for (path, old) in &existing {
        if !current_paths.contains(path.as_str()) {
            tx.execute("delete from files where id=?1", [old.id])?;
        }
    }

    let mut pending: Vec<Option<Pending>> = (0..files.len()).map(|_| None).collect();
    let mut changed = Vec::new();
    let mut reused = 0usize;
    for (index, file) in files.iter().enumerate() {
        let cached = existing
            .get(&file.rel)
            .filter(|old| old.hash == file.hash)
            .and_then(|old| load_cached(&tx, file, old.id).transpose())
            .transpose()?;

        if let Some(cached) = cached {
            let old = existing
                .get(&file.rel)
                .context("cached file missing from existing rows")?;
            if old.mtime != file.mtime || old.size != file.size {
                tx.execute(
                    "update files set mtime=?1, size=?2 where id=?3",
                    rusqlite::params![file.mtime, file.size, cached.file_id],
                )?;
            }
            pending[index] = Some(cached);
            reused += 1;
            continue;
        }

        if let Some(old) = existing.get(&file.rel) {
            tx.execute("delete from files where id=?1", [old.id])?;
        }
        changed.push(index);
    }

    // Parsing is CPU-bound and owns no database state. Each bounded worker gets
    // its own Tree-sitter parsers/queries; rows are still inserted serially in
    // sorted file order so IDs and exported graph output remain deterministic.
    let parsed_files = extract_changed(&files, &changed, requested_jobs)?;
    for (index, extracted) in parsed_files {
        pending[index] = Some(index_extracted(&tx, repo_id, &files[index], extracted)?);
    }
    let pending: Vec<Pending> = pending
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .context("missing extraction after parallel parse")?;
    let parsed = changed.len();

    let mut by_name: HashMap<String, Vec<i64>> = HashMap::new();
    let mut hcl_by_dir: HashMap<&Path, HashMap<String, Vec<i64>>> = HashMap::new();
    let mut by_container: HashMap<(String, String), Vec<i64>> = HashMap::new();
    let mut file_symbol: HashMap<String, i64> = HashMap::new();
    let mut file_rows: HashMap<String, i64> = HashMap::new();
    let mut n_symbols = 0usize;
    for file in &pending {
        file_symbol.insert(file.rel.clone(), file.file_symbol);
        file_rows.insert(file.rel.clone(), file.file_id);
        n_symbols += 1 + file.symbol_ids.len();
        for (index, symbol) in file.extracted.symbols.iter().enumerate() {
            let Some(&id) = file.symbol_ids.get(index) else {
                continue;
            };
            by_name.entry(symbol.name.clone()).or_default().push(id);
            if file.lang == Lang::Hcl {
                let directory = Path::new(&file.rel).parent().unwrap_or(Path::new(""));
                hcl_by_dir
                    .entry(directory)
                    .or_default()
                    .entry(symbol.name.clone())
                    .or_default()
                    .push(id);
            }
            if let Some(container) = file.extracted.containers.get(index).cloned().flatten() {
                by_container
                    .entry((container, symbol.name.clone()))
                    .or_default()
                    .push(id);
            }
        }
    }

    let mut n_edges = 0usize;
    let mut insert_edge = tx.prepare(
        "insert or ignore into edges(repo_id, src_symbol_id, dst_symbol_id, kind)
         values (?1, ?2, ?3, ?4)",
    )?;
    for file in &pending {
        for (index, &id) in file.symbol_ids.iter().enumerate() {
            let parent = file
                .extracted
                .parents
                .get(index)
                .copied()
                .flatten()
                .and_then(|parent| file.symbol_ids.get(parent).copied())
                .unwrap_or(file.file_symbol);
            n_edges += insert_edge.execute(rusqlite::params![repo_id, parent, id, "contains"])?;
        }
    }

    let mut external: HashMap<String, i64> = HashMap::new();
    let go_module = go_module_path(root);
    let active_gitlab = crate::gitlab_ci::active_paths(&pending);
    for file in &pending {
        if file.extracted.automation.dialect == crate::yaml::Dialect::GitLab
            && !active_gitlab.contains(file.rel.as_str())
        {
            continue;
        }
        for spec in &file.extracted.imports {
            let mut targets = resolve_import(
                &file.rel,
                file.lang,
                spec,
                go_module.as_deref(),
                &file_symbol,
            );
            if targets.is_empty() {
                let target = if let Some(&id) = external.get(spec) {
                    id
                } else {
                    let anchor = *file_rows
                        .get(&file.rel)
                        .context("missing file row for external import")?;
                    tx.execute(
                        "insert into symbols(repo_id, file_id, name, kind, start_line, end_line, signature)
                         values (?1, ?2, ?3, 'module', 0, 0, ?3)",
                        rusqlite::params![repo_id, anchor, spec],
                    )?;
                    let id = tx.last_insert_rowid();
                    external.insert(spec.clone(), id);
                    n_symbols += 1;
                    id
                };
                targets.push(target);
            }
            for target in targets {
                n_edges += insert_edge.execute(rusqlite::params![
                    repo_id,
                    file.file_symbol,
                    target,
                    "imports"
                ])?;
            }
        }
    }

    let mut unresolved = 0usize;
    for file in &pending {
        let mut bindings: HashMap<(Option<usize>, &str), &str> = HashMap::new();
        for binding in &file.extracted.bindings {
            bindings.insert((binding.owner, binding.name.as_str()), binding.ty.as_str());
        }

        for call in &file.extracted.calls {
            let from_id = call
                .from
                .and_then(|index| file.symbol_ids.get(index).copied())
                .unwrap_or(file.file_symbol);

            if let Some(receiver) = &call.receiver {
                let ty = receiver_type(&file.extracted, &bindings, call, receiver);
                if let Some(ty) = ty
                    && let Some([only]) = by_container
                        .get(&(ty, call.callee.clone()))
                        .map(Vec::as_slice)
                {
                    n_edges +=
                        insert_edge.execute(rusqlite::params![repo_id, from_id, *only, "calls"])?;
                    continue;
                }
                unresolved += 1;
                continue;
            }

            match if file.lang == Lang::Hcl {
                let directory = Path::new(&file.rel).parent().unwrap_or(Path::new(""));
                hcl_by_dir
                    .get(directory)
                    .and_then(|names| resolve_hcl_callee(names, from_id, &call.callee))
            } else if file.lang == Lang::Yaml {
                let matches: Vec<_> = file
                    .extracted
                    .symbols
                    .iter()
                    .enumerate()
                    .filter(|(_, symbol)| symbol.kind == "anchor" && symbol.name == call.callee)
                    .map(|(index, _)| file.symbol_ids[index])
                    .collect();
                if let [only] = matches.as_slice() {
                    Some(*only)
                } else {
                    None
                }
            } else {
                extract::resolve(&by_name, from_id, &call.callee)
            } {
                Some(target) => {
                    n_edges += insert_edge
                        .execute(rusqlite::params![repo_id, from_id, target, "calls"])?;
                }
                None => unresolved += 1,
            }
        }
    }

    let mut automation = crate::ansible::resolve(&pending, &file_symbol);
    let actions = crate::github_actions::resolve(&pending, &file_symbol, &external);
    automation.edges.extend(actions.edges);
    automation.unresolved += actions.unresolved;
    let compose = crate::compose::resolve(&pending, &file_symbol);
    automation.edges.extend(compose.edges);
    automation.unresolved += compose.unresolved;
    let kube = crate::kubernetes::resolve(&pending);
    automation.edges.extend(kube.edges);
    automation.unresolved += kube.unresolved;
    let kustomize = crate::kustomize::resolve(&pending, &file_symbol);
    automation.edges.extend(kustomize.edges);
    automation.unresolved += kustomize.unresolved;
    let gitlab = crate::gitlab_ci::resolve(&pending, &file_symbol, &external);
    automation.edges.extend(gitlab.edges);
    automation.unresolved += gitlab.unresolved;
    unresolved += automation.unresolved;
    for (from, target, kind) in automation.edges {
        n_edges += insert_edge.execute(rusqlite::params![repo_id, from, target, kind])?;
    }

    drop(insert_edge);
    tx.commit().context("commit build")?;
    Ok(BuildStats {
        files: files.len(),
        symbols: n_symbols,
        edges: n_edges,
        unresolved,
        parsed,
        reused,
        deleted,
    })
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn load_existing_files(
    tx: &rusqlite::Transaction<'_>,
    repo_id: i64,
) -> Result<HashMap<String, ExistingFile>> {
    let mut statement =
        tx.prepare("select id, path, hash, mtime, size from files where repo_id=?1")?;
    let rows = statement.query_map([repo_id], |row| {
        Ok((
            row.get::<_, String>(1)?,
            ExistingFile {
                id: row.get(0)?,
                hash: row.get(2)?,
                mtime: row.get(3)?,
                size: row.get(4)?,
            },
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn load_cached(
    tx: &rusqlite::Transaction<'_>,
    source: &SourceFile,
    file_id: i64,
) -> Result<Option<Pending>> {
    use rusqlite::OptionalExtension;

    let payload: Option<String> = tx
        .query_row(
            "select payload from file_extracts
              where file_id=?1 and extractor_stamp=?2",
            rusqlite::params![file_id, EXTRACTOR_STAMP],
            |row| row.get(0),
        )
        .optional()?;
    let Some(payload) = payload else {
        return Ok(None);
    };
    let Ok(extracted) = serde_json::from_str::<extract::Extracted>(&payload) else {
        return Ok(None);
    };
    let file_symbol: Option<i64> = tx
        .query_row(
            "select id from symbols where file_id=?1 and kind='file'",
            [file_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(file_symbol) = file_symbol else {
        return Ok(None);
    };

    let mut statement = tx.prepare(
        "select id from symbols
          where file_id=?1 and kind not in ('file','module')
          order by id",
    )?;
    let symbol_ids: Vec<i64> = statement
        .query_map([file_id], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    if symbol_ids.len() != extracted.symbols.len() {
        return Ok(None);
    }

    Ok(Some(Pending {
        rel: source.rel.clone(),
        lang: source.lang,
        file_id,
        file_symbol,
        symbol_ids,
        extracted,
    }))
}

const MIN_FILES_PER_WORKER: usize = 64;

fn extract_changed(
    files: &[SourceFile],
    indexes: &[usize],
    requested_jobs: usize,
) -> Result<Vec<(usize, extract::Extracted)>> {
    if indexes.is_empty() {
        return Ok(Vec::new());
    }
    let useful_workers = indexes.len().div_ceil(MIN_FILES_PER_WORKER).max(1);
    let jobs = requested_jobs.clamp(1, 32).min(useful_workers);
    let chunk_size = indexes.len().div_ceil(jobs);

    if jobs == 1 {
        let mut extractor = extract::Extractor::new();
        return indexes
            .iter()
            .map(|&index| {
                extractor
                    .extract_file(files[index].lang, &files[index].text, &files[index].rel)
                    .with_context(|| format!("extract {}", files[index].rel))
                    .map(|extracted| (index, extracted))
            })
            .collect();
    }

    std::thread::scope(|scope| {
        let mut workers = Vec::with_capacity(jobs);
        for chunk in indexes.chunks(chunk_size) {
            workers.push(
                scope.spawn(move || -> Result<Vec<(usize, extract::Extracted)>> {
                    let mut extractor = extract::Extractor::new();
                    chunk
                        .iter()
                        .map(|&index| {
                            extractor
                                .extract_file(
                                    files[index].lang,
                                    &files[index].text,
                                    &files[index].rel,
                                )
                                .with_context(|| format!("extract {}", files[index].rel))
                                .map(|extracted| (index, extracted))
                        })
                        .collect()
                }),
            );
        }

        let mut extracted = Vec::with_capacity(indexes.len());
        for worker in workers {
            let batch = worker
                .join()
                .map_err(|_| anyhow!("extraction worker panicked"))??;
            extracted.extend(batch);
        }
        Ok(extracted)
    })
}

fn index_extracted(
    tx: &rusqlite::Transaction<'_>,
    repo_id: i64,
    source: &SourceFile,
    extracted: extract::Extracted,
) -> Result<Pending> {
    let file_id = insert_file(tx, repo_id, source)?;
    let lines = source.text.lines().count().max(1) as i64;
    tx.execute(
        "insert into symbols(repo_id, file_id, name, kind, start_line, end_line, signature, summary)
         values (?1, ?2, ?3, 'file', 1, ?4, ?3, ?5)",
        rusqlite::params![repo_id, file_id, source.rel, lines, bounded_text(&source.text)],
    )?;
    let file_symbol = tx.last_insert_rowid();
    crate::search::index_symbol(tx, file_symbol)?;
    let mut symbol_ids = Vec::with_capacity(extracted.symbols.len());
    for (index, symbol) in extracted.symbols.iter().enumerate() {
        let container = extracted.containers.get(index).cloned().flatten();
        tx.execute(
            "insert into symbols(repo_id, file_id, name, kind, start_line, end_line, signature, container, summary)
             values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                repo_id,
                file_id,
                symbol.name,
                symbol.kind,
                symbol.start_line,
                symbol.end_line,
                symbol.signature,
                container,
                symbol.search_text
            ],
        )?;
        let id = tx.last_insert_rowid();
        crate::search::index_symbol(tx, id)?;
        symbol_ids.push(id);
    }
    let payload = serde_json::to_string(&extracted).context("serialize extraction cache")?;
    tx.execute(
        "insert into file_extracts(file_id, extractor_stamp, payload)
         values (?1, ?2, ?3)",
        rusqlite::params![file_id, EXTRACTOR_STAMP, payload],
    )?;

    Ok(Pending {
        rel: source.rel.clone(),
        lang: source.lang,
        file_id,
        file_symbol,
        symbol_ids,
        extracted,
    })
}

fn bounded_text(text: &str) -> &str {
    const LIMIT: usize = 32 * 1024;
    if text.len() <= LIMIT {
        return text;
    }
    let mut end = LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn receiver_type(
    extracted: &extract::Extracted,
    bindings: &HashMap<(Option<usize>, &str), &str>,
    call: &extract::CallIntent,
    receiver: &str,
) -> Option<String> {
    if receiver == "this" || receiver == "self" {
        return call
            .from
            .and_then(|index| extracted.containers.get(index).cloned().flatten());
    }
    if let Some(field) = receiver
        .strip_prefix("this.")
        .or_else(|| receiver.strip_prefix("self."))
    {
        let class = call
            .from
            .and_then(|index| extracted.containers.get(index).cloned().flatten())?;
        let class_index = extracted
            .symbols
            .iter()
            .position(|symbol| symbol.name == class && symbol.kind == "class")?;
        return bindings
            .get(&(Some(class_index), format!("this.{field}").as_str()))
            .or_else(|| bindings.get(&(Some(class_index), field)))
            .copied()
            .map(str::to_string);
    }
    bindings
        .get(&(call.from, receiver))
        .or_else(|| bindings.get(&(None, receiver)))
        .copied()
        .map(str::to_string)
}

/// Lexically normalize a repo-relative path without consulting the filesystem.
fn normalize_path(path: &Path) -> String {
    let joined = path.to_string_lossy();
    // Lexical normalisation: `a/b/../c` -> `a/c`, without touching the disk.
    let mut parts: Vec<&str> = Vec::new();
    for c in joined.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Resolve one language's import to file nodes in the same repo.
///
/// Go packages may contain multiple files, so this returns every file node in the
/// package. TypeScript and Python module paths resolve to at most one file.
fn resolve_import(
    from_rel: &str,
    lang: Lang,
    spec: &str,
    go_module: Option<&str>,
    files: &HashMap<String, i64>,
) -> Vec<i64> {
    let from_dir = Path::new(from_rel).parent().unwrap_or(Path::new(""));
    let candidates = match lang {
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            if !spec.starts_with('.') {
                return Vec::new();
            }
            let stem = normalize_path(&from_dir.join(spec));
            let base = stem.strip_suffix(".js").unwrap_or(&stem);
            vec![
                format!("{base}.ts"),
                format!("{base}.tsx"),
                format!("{base}.js"),
                format!("{base}.mjs"),
                format!("{base}.cjs"),
                stem.clone(),
                format!("{base}/index.ts"),
                format!("{base}/index.tsx"),
                format!("{base}/index.js"),
                format!("{base}/index.mjs"),
                format!("{base}/index.cjs"),
            ]
        }
        Lang::Python => {
            let dots = spec.bytes().take_while(|b| *b == b'.').count();
            let module = spec[dots..].replace('.', "/");
            let mut base = from_dir.to_path_buf();
            if dots == 0 {
                base = Path::new("").to_path_buf();
            } else {
                for _ in 1..dots {
                    base.pop();
                }
            }
            let stem = normalize_path(&base.join(module));
            vec![format!("{stem}.py"), format!("{stem}/__init__.py")]
        }
        Lang::Go => {
            let Some(module) = go_module else {
                return Vec::new();
            };
            let package = if spec == module {
                ""
            } else if let Some(rest) = spec.strip_prefix(module).and_then(|s| s.strip_prefix('/')) {
                rest
            } else {
                return Vec::new();
            };
            let mut ids: Vec<i64> = files
                .iter()
                .filter(|(path, _)| {
                    path.ends_with(".go")
                        && Path::new(path.as_str()).parent().unwrap_or(Path::new(""))
                            == Path::new(package)
                })
                .map(|(_, id)| *id)
                .collect();
            ids.sort_unstable();
            return ids;
        }
        Lang::Rust => return resolve_rust_import(from_rel, spec, files),
        Lang::Shell => {
            if spec.contains('$') || spec.contains('`') {
                return Vec::new();
            }
            vec![
                normalize_path(&from_dir.join(spec)),
                normalize_path(Path::new(spec)),
            ]
        }
        Lang::Hcl => {
            if !(spec.starts_with("./") || spec.starts_with("../")) {
                return Vec::new();
            }
            let dir = normalize_path(&from_dir.join(spec));
            let mut ids: Vec<i64> = files
                .iter()
                .filter(|(path, _)| {
                    let path = Path::new(path.as_str());
                    let configuration = if matches!(
                        Path::new(from_rel).extension().and_then(|ext| ext.to_str()),
                        Some("tf" | "tofu")
                    ) {
                        matches!(
                            path.extension().and_then(|ext| ext.to_str()),
                            Some("tf" | "tofu")
                        )
                    } else {
                        Lang::of_path(path) == Some(Lang::Hcl)
                    };
                    configuration && path.parent().unwrap_or(Path::new("")) == Path::new(&dir)
                })
                .map(|(_, id)| *id)
                .collect();
            ids.sort_unstable();
            return ids;
        }
        // Local YAML automation links have context-specific resolution rules.
        // The plain imports list contains only external action references.
        Lang::Yaml => return Vec::new(),
    };
    for candidate in candidates {
        if let Some(&id) = files.get(&candidate) {
            return vec![id];
        }
    }
    Vec::new()
}

/// Terraform refs often include a computed suffix (`aws_instance.web.id`).
/// Exact match first, then strip trailing segments until a unique symbol hits.
fn resolve_hcl_callee(
    by_name: &HashMap<String, Vec<i64>>,
    from_id: i64,
    callee: &str,
) -> Option<i64> {
    if let Some(target) = extract::resolve(by_name, from_id, callee) {
        return Some(target);
    }
    // A missing alias must not silently resolve to the default provider.
    if callee.starts_with("provider.") {
        return None;
    }
    let mut current = callee;
    while let Some((prefix, _)) = current.rsplit_once('.') {
        if let Some(target) = extract::resolve(by_name, from_id, prefix) {
            return Some(target);
        }
        current = prefix;
    }
    None
}

fn resolve_rust_import(from_rel: &str, spec: &str, files: &HashMap<String, i64>) -> Vec<i64> {
    let from_dir = Path::new(from_rel).parent().unwrap_or(Path::new(""));
    let crate_dir = if let Some(index) = from_rel.rfind("/src/") {
        Path::new(&from_rel[..index + 4]).to_path_buf()
    } else if from_rel.starts_with("src/") {
        Path::new("src").to_path_buf()
    } else {
        from_dir.to_path_buf()
    };

    let mut raw = spec
        .split(" as ")
        .next()
        .unwrap_or(spec)
        .split('{')
        .next()
        .unwrap_or(spec)
        .trim_end_matches(':')
        .trim_end_matches("::*")
        .to_string();
    let (base, explicitly_local) = if let Some(rest) = raw.strip_prefix("crate::") {
        raw = rest.to_string();
        (crate_dir, true)
    } else if let Some(rest) = raw.strip_prefix("self::") {
        raw = rest.to_string();
        (from_dir.to_path_buf(), true)
    } else if let Some(rest) = raw.strip_prefix("super::") {
        raw = rest.to_string();
        let mut base = from_dir.to_path_buf();
        base.pop();
        (base, true)
    } else {
        (from_dir.to_path_buf(), !raw.contains("::"))
    };
    if !explicitly_local || raw.is_empty() {
        return Vec::new();
    }

    let segments: Vec<&str> = raw
        .split("::")
        .filter(|segment| !segment.is_empty() && *segment != "self")
        .collect();
    for length in (1..=segments.len()).rev() {
        let module = normalize_path(&base.join(segments[..length].join("/")));
        for candidate in [format!("{module}.rs"), format!("{module}/mod.rs")] {
            if let Some(&id) = files.get(&candidate) {
                return vec![id];
            }
        }
    }
    Vec::new()
}

fn go_module_path(root: &Path) -> Option<String> {
    let text = std::fs::read_to_string(root.join("go.mod")).ok()?;
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("module ")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn insert_file(tx: &rusqlite::Transaction, repo_id: i64, f: &SourceFile) -> Result<i64> {
    tx.execute(
        "insert into files(repo_id, path, mtime, size, hash) values (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![repo_id, f.rel, f.mtime, f.size, f.hash],
    )?;
    Ok(tx.last_insert_rowid())
}

/// One occurrence of the pattern, with the line it sits on.
#[derive(Debug, Serialize)]
pub struct Occurrence {
    pub line: i64,
    pub text: String,
}

/// Occurrences grouped under the symbol that encloses them.
#[derive(Debug, Serialize)]
pub struct Group {
    pub path: String,
    /// None for a match outside any definition — imports, top-level config.
    pub symbol: Option<String>,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub in_edges: i64,
    pub hits: Vec<Occurrence>,
}

#[derive(Debug, Serialize)]
pub struct GrepResult {
    pub groups: Vec<Group>,
    pub total_hits: usize,
    pub files_searched: usize,
    /// Indexed files that could not be re-read. Surfaced rather than swallowed:
    /// a silent skip makes an incomplete search look exhaustive.
    pub unreadable: usize,
}

struct SpanRow {
    id: i64,
    name: String,
    kind: String,
    start: i64,
    end: i64,
    in_edges: i64,
}

/// Every occurrence of a literal, grouped by enclosing symbol.
///
/// Searches file *contents*, not stored symbol metadata. `grep` answers "where is
/// this used", so the definition is the least interesting of its hits — matching
/// only names would return one line and hide the fourteen call sites that
/// actually matter for a change.
///
/// Files are re-read from the work tree rather than duplicated into the store.
/// The store holds structure; the source is already on disk, and copying it would
/// double the database for data that goes stale the moment anyone edits.
pub struct GrepOptions<'a> {
    pub ignore_case: bool,
    pub fixed: bool,
    pub scope: Option<&'a str>,
}

#[cfg(test)]
pub fn grep(db: &Connection, repo_id: i64, root: &Path, needle: &str) -> Result<GrepResult> {
    grep_with_options(
        db,
        repo_id,
        root,
        needle,
        GrepOptions {
            ignore_case: false,
            fixed: true,
            scope: None,
        },
    )
}

pub fn grep_with_options(
    db: &Connection,
    repo_id: i64,
    root: &Path,
    pattern: &str,
    options: GrepOptions<'_>,
) -> Result<GrepResult> {
    let expression = if options.fixed {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    let matcher = regex::RegexBuilder::new(&expression)
        .case_insensitive(options.ignore_case)
        .build()
        .with_context(|| format!("invalid regex {pattern:?}"))?;
    // Symbol spans per file, so a matching line can be attributed to a definition.
    let mut spans: HashMap<String, Vec<SpanRow>> = HashMap::new();
    let mut stmt = db.prepare(
        "select f.path, s.id, s.name, s.kind, s.start_line, s.end_line,
                (select count(*) from edges e
                  where e.repo_id = s.repo_id and e.dst_symbol_id = s.id)
           from symbols s join files f on f.id = s.file_id
          where s.repo_id = ?1 and s.kind not in ('file','module')",
    )?;
    let mut rows = stmt.query([repo_id])?;
    while let Some(r) = rows.next()? {
        let path: String = r.get(0)?;
        spans.entry(path).or_default().push(SpanRow {
            id: r.get(1)?,
            name: r.get(2)?,
            kind: r.get(3)?,
            start: r.get(4)?,
            end: r.get(5)?,
            in_edges: r.get(6)?,
        });
    }

    let mut files_stmt = db.prepare("select path from files where repo_id=?1 order by path")?;
    let paths: Vec<String> = files_stmt
        .query_map([repo_id], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|path| options.scope.is_none_or(|scope| path_in_scope(path, scope)))
        .collect();

    let mut groups: Vec<Group> = Vec::new();
    let mut total_hits = 0usize;
    let mut unreadable = 0usize;

    for path in &paths {
        let Ok(text) = std::fs::read_to_string(root.join(path)) else {
            unreadable += 1;
            continue;
        };
        // (symbol key) -> group index, so occurrences accumulate per definition.
        let mut seen: HashMap<Option<i64>, usize> = HashMap::new();
        for (i, line_text) in text.lines().enumerate() {
            if !matcher.is_match(line_text) {
                continue;
            }
            let line = i as i64 + 1;
            // Innermost enclosing definition: narrowest span containing the line.
            let owner = spans.get(path).and_then(|v| {
                v.iter()
                    .filter(|span| span.start <= line && line <= span.end)
                    .min_by_key(|span| span.end - span.start)
            });
            let key = owner.map(|span| span.id);
            let idx = match seen.get(&key) {
                Some(&i) => i,
                None => {
                    groups.push(Group {
                        path: path.clone(),
                        symbol: owner.map(|span| span.name.clone()),
                        kind: owner
                            .map(|span| span.kind.clone())
                            .unwrap_or_else(|| "file".into()),
                        start_line: owner.map(|span| span.start).unwrap_or(0),
                        end_line: owner.map(|span| span.end).unwrap_or(0),
                        in_edges: owner.map(|span| span.in_edges).unwrap_or(0),
                        hits: Vec::new(),
                    });
                    seen.insert(key, groups.len() - 1);
                    groups.len() - 1
                }
            };
            groups[idx].hits.push(Occurrence {
                line,
                text: line_text.trim().to_string(),
            });
            total_hits += 1;
        }
    }

    // Most-referenced definitions first: the symbol other code depends on is the
    // one a reader is usually looking for.
    groups.sort_by(|a, b| {
        b.in_edges
            .cmp(&a.in_edges)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });

    Ok(GrepResult {
        groups,
        total_hits,
        files_searched: paths.len(),
        unreadable,
    })
}

pub fn path_in_scope(path: &str, scope: &str) -> bool {
    let scope = scope.trim_matches('/');
    scope.is_empty() || path == scope || path.starts_with(&format!("{scope}/"))
}

pub fn repo_id_of(db: &Connection, root: &Path) -> Result<Option<i64>> {
    let mut stmt = db.prepare("select id from repos where root=?1 and extractor_stamp=?2")?;
    let mut rows = stmt.query(rusqlite::params![root.to_string_lossy(), EXTRACTOR_STAMP])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get(0)?),
        None => None,
    })
}

#[derive(Debug, Serialize)]
pub struct RepoStatus {
    pub files: i64,
    pub symbols: i64,
    pub edges: i64,
    pub extractor_stamp: String,
    pub indexed_at: i64,
}

pub fn repo_status(db: &Connection, repo_id: i64) -> Result<RepoStatus> {
    db.query_row(
        "select
            (select count(*) from files where repo_id=?1),
            (select count(*) from symbols where repo_id=?1),
            (select count(*) from edges where repo_id=?1),
            extractor_stamp, indexed_at
           from repos where id=?1",
        [repo_id],
        |row| {
            Ok(RepoStatus {
                files: row.get(0)?,
                symbols: row.get(1)?,
                edges: row.get(2)?,
                extractor_stamp: row.get(3)?,
                indexed_at: row.get(4)?,
            })
        },
    )
    .context("read repository status")
}

/// One step away from the seed, with the distance that reached it.
#[derive(Debug, Serialize)]
pub struct Reached {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub edge: String,
    pub depth: usize,
    pub signature: String,
    pub in_edges: i64,
}

#[derive(Debug, Serialize)]
pub struct Seed {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
}

/// Breadth-first traversal of the edge set from every symbol named `name`.
///
/// `Out` follows src->dst (what this calls); the default follows dst->src (what
/// calls this). Breadth-first and visited-guarded, so a cycle terminates and each
/// symbol is reported at its shortest distance rather than once per path.
#[cfg(test)]
pub fn callers(
    db: &Connection,
    repo_id: i64,
    name: &str,
    out: bool,
    max_depth: usize,
) -> Result<(Vec<Seed>, Vec<Reached>)> {
    callers_scoped(db, repo_id, name, out, max_depth, None)
}

pub fn callers_scoped(
    db: &Connection,
    repo_id: i64,
    qualified_name: &str,
    out: bool,
    max_depth: usize,
    scope: Option<&str>,
) -> Result<(Vec<Seed>, Vec<Reached>)> {
    let (container, name) = if qualified_name.contains('/') {
        (None, qualified_name)
    } else {
        qualified_name
            .rsplit_once('.')
            .map_or((None, qualified_name), |(container, name)| {
                (Some(container), name)
            })
    };
    let container_tail = container.and_then(|container| container.rsplit('.').next());
    let mut seeds_stmt = db.prepare(
        "select s.id, s.name, s.kind, f.path, s.start_line, s.end_line
           from symbols s join files f on f.id = s.file_id
          where s.repo_id = ?1 and lower(s.name) = lower(?2)
            and (?3 is null or lower(s.container) in (lower(?3), lower(?4)))
          order by f.path, s.start_line",
    )?;
    let mut lookup =
        |name: &str, container: Option<&str>, container_tail: Option<&str>| -> Result<Vec<Seed>> {
            Ok(seeds_stmt
                .query_map(
                    rusqlite::params![repo_id, name, container, container_tail],
                    |r| {
                        Ok(Seed {
                            id: r.get(0)?,
                            name: r.get(1)?,
                            kind: r.get(2)?,
                            path: r.get(3)?,
                            start_line: r.get(4)?,
                            end_line: r.get(5)?,
                        })
                    },
                )?
                .filter_map(|row| match row {
                    Ok(seed) if scope.is_none_or(|scope| path_in_scope(&seed.path, scope)) => {
                        Some(Ok(seed))
                    }
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                })
                .collect::<Result<_, _>>()?)
        };
    // YAML, automation, and HCL definitions use literal dotted names. Prefer
    // an exact definition before interpreting dots as class qualification.
    let mut seeds = lookup(qualified_name, None, None)?;
    if seeds.is_empty() && container.is_some() {
        seeds = lookup(name, container, container_tail)?;
    }

    let (from_col, to_col) = if out {
        ("src_symbol_id", "dst_symbol_id")
    } else {
        ("dst_symbol_id", "src_symbol_id")
    };
    let sql = format!(
        "select s.id, s.name, s.kind, f.path, s.start_line, s.end_line, e.kind,
                coalesce(s.signature,''),
                (select count(*) from edges incoming
                  where incoming.repo_id=s.repo_id and incoming.dst_symbol_id=s.id)
           from edges e
           join symbols s on s.id = e.{to_col}
           join files f on f.id = s.file_id
          where e.repo_id = ?1 and e.{from_col} = ?2 and e.kind != 'contains'
          order by f.path, s.start_line"
    );
    let mut step = db.prepare(&sql)?;

    let mut seen: std::collections::HashSet<i64> = seeds.iter().map(|s| s.id).collect();
    let mut frontier: Vec<i64> = seeds.iter().map(|s| s.id).collect();
    if max_depth > 1 {
        let mut file_members = db.prepare(
            "select member.id
               from symbols file
               join symbols member on member.file_id = file.file_id
              where file.repo_id=?1 and file.id=?2
                and file.kind='file' and member.kind not in ('file','module')",
        )?;
        for seed in &seeds {
            if seed.kind != "file" {
                continue;
            }
            let ids = file_members.query_map(rusqlite::params![repo_id, seed.id], |r| r.get(0))?;
            for id in ids {
                let id = id?;
                if seen.insert(id) {
                    frontier.push(id);
                }
            }
        }
    }
    let mut reached = Vec::new();

    for depth in 1..=max_depth {
        let mut next = Vec::new();
        for id in &frontier {
            let rows = step.query_map(rusqlite::params![repo_id, id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    Reached {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        kind: r.get(2)?,
                        path: r.get(3)?,
                        start_line: r.get(4)?,
                        end_line: r.get(5)?,
                        edge: r.get(6)?,
                        depth,
                        signature: r.get(7)?,
                        in_edges: r.get(8)?,
                    },
                ))
            })?;
            for row in rows {
                let (id, hit) = row?;
                // First arrival wins: BFS means that is the shortest distance.
                if seen.insert(id) {
                    next.push(id);
                    if scope.is_none_or(|scope| path_in_scope(&hit.path, scope)) {
                        reached.push(hit);
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok((seeds, reached))
}

#[derive(Debug, Serialize)]
pub struct SkelRow {
    pub name: String,
    pub kind: String,
    pub start_line: i64,
    pub end_line: i64,
    pub signature: String,
    pub container: Option<String>,
}

/// Every signature in one file, ordered as they appear.
///
/// Excludes the synthetic file node itself: the caller already named the file, and
/// listing it as a member of itself is noise.
pub fn skeleton(db: &Connection, repo_id: i64, rel: &str) -> Result<Vec<SkelRow>> {
    let mut stmt = db.prepare(
        "select s.name, s.kind, s.start_line, s.end_line, coalesce(s.signature,''), s.container
           from symbols s join files f on f.id = s.file_id
          where s.repo_id = ?1 and f.path = ?2 and s.kind not in ('file','module')
          order by s.start_line, s.end_line desc",
    )?;
    Ok(stmt
        .query_map(rusqlite::params![repo_id, rel], |r| {
            Ok(SkelRow {
                name: r.get(0)?,
                kind: r.get(1)?,
                start_line: r.get(2)?,
                end_line: r.get(3)?,
                signature: r.get(4)?,
                container: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn skeleton_path(db: &Connection, repo_id: i64, requested: &str) -> Result<Option<String>> {
    let requested = requested.trim_start_matches("./");
    let mut exact = db.prepare("select path from files where repo_id=?1 and path=?2")?;
    if let Some(path) = exact
        .query_map(rusqlite::params![repo_id, requested], |row| row.get(0))?
        .next()
        .transpose()?
    {
        return Ok(Some(path));
    }
    if requested.contains('/') {
        return Ok(None);
    }
    let mut all = db.prepare("select path from files where repo_id=?1 order by path")?;
    let matches = all
        .query_map([repo_id], |row| row.get::<_, String>(0))?
        .filter_map(|row| match row {
            Ok(path)
                if Path::new(&path)
                    .file_name()
                    .is_some_and(|name| name == requested) =>
            {
                Some(Ok(path))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((matches.len() == 1).then(|| matches[0].clone()))
}

#[derive(Debug, Serialize)]
pub struct Hub {
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub in_degree: i64,
}

#[derive(Debug, Serialize)]
pub struct DirEntry {
    pub dir: String,
    pub files: i64,
    pub symbols: i64,
    pub hubs: Vec<Hub>,
}

#[derive(Debug, Serialize)]
pub struct RepoMap {
    pub files: i64,
    pub symbols: i64,
    pub edges: i64,
    pub dirs: Vec<DirEntry>,
    pub dropped_dirs: usize,
    pub hotspots: Vec<Hub>,
}

/// In-degree over `calls` edges only. Containment would rank every symbol at
/// exactly 1 and imports would rank files, neither of which says "many things
/// depend on this" — which is the question a hub answers.
const HUB_SQL: &str = "select s.name, s.kind, f.path, s.start_line, s.end_line,
        (select count(*) from edges e
          where e.repo_id = s.repo_id and e.dst_symbol_id = s.id and e.kind = 'calls') d
   from symbols s join files f on f.id = s.file_id
  where s.repo_id = ?1 and s.kind not in ('file','module')";

fn read_hubs(stmt: &mut rusqlite::Statement, repo_id: i64) -> Result<Vec<Hub>> {
    Ok(stmt
        .query_map([repo_id], |r| {
            Ok(Hub {
                name: r.get(0)?,
                kind: r.get(1)?,
                path: r.get(2)?,
                start_line: r.get(3)?,
                end_line: r.get(4)?,
                in_degree: r.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

/// Orientation for an unfamiliar repo: directory clusters, their hubs, and the
/// most-depended-on symbols overall.
pub fn repo_map(db: &Connection, repo_id: i64, top: usize) -> Result<RepoMap> {
    let (files, symbols, edges): (i64, i64, i64) = db.query_row(
        "select (select count(*) from files where repo_id=?1),
                (select count(*) from symbols where repo_id=?1 and kind not in ('file','module')),
                (select count(*) from edges where repo_id=?1)",
        [repo_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;

    let mut stmt = db.prepare(&format!("{HUB_SQL} order by d desc, s.name"))?;
    let all = read_hubs(&mut stmt, repo_id)?;

    // Top-level directory, or "." for a file at the root.
    let bucket = |p: &str| -> String {
        match p.split_once('/') {
            Some((head, _)) => format!("{head}/"),
            None => ".".to_string(),
        }
    };

    let mut per_dir: HashMap<String, (std::collections::HashSet<String>, i64, Vec<&Hub>)> =
        HashMap::new();
    let mut file_stmt = db.prepare("select path from files where repo_id=?1 order by path")?;
    let paths = file_stmt.query_map([repo_id], |r| r.get::<_, String>(0))?;
    for path in paths {
        let path = path?;
        per_dir.entry(bucket(&path)).or_default().0.insert(path);
    }
    for h in &all {
        let e = per_dir.entry(bucket(&h.path)).or_default();
        e.1 += 1;
        if h.in_degree > 0 && e.2.len() < 3 {
            e.2.push(h);
        }
    }

    let mut dirs: Vec<DirEntry> = per_dir
        .into_iter()
        .map(|(dir, (fs, n, hubs))| DirEntry {
            dir,
            files: fs.len() as i64,
            symbols: n,
            hubs: hubs
                .into_iter()
                .map(|h| Hub {
                    name: h.name.clone(),
                    kind: h.kind.clone(),
                    path: h.path.clone(),
                    start_line: h.start_line,
                    end_line: h.end_line,
                    in_degree: h.in_degree,
                })
                .collect(),
        })
        .collect();
    dirs.sort_by(|a, b| b.symbols.cmp(&a.symbols).then_with(|| a.dir.cmp(&b.dir)));
    let dropped_dirs = dirs.len().saturating_sub(top);
    dirs.truncate(top);

    let hotspots = all
        .into_iter()
        .filter(|h| h.in_degree > 0)
        .take(top)
        .collect();
    Ok(RepoMap {
        files,
        symbols,
        edges,
        dirs,
        dropped_dirs,
        hotspots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a throwaway repo on disk, index it, and hand back (store, root).
    fn fixture(files: &[(&str, &str)]) -> (Connection, PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "panoptes-idx-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, body) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let mut conn = crate::db::open(&root.join("store.db")).unwrap();
        crate::index::build(&mut conn, &root).unwrap();
        (conn, root)
    }

    #[test]
    fn grep_finds_call_sites_not_just_the_definition() {
        // The whole point of grep: a definition plus every place that uses it.
        // Matching stored symbol names alone returns 1 hit and hides the callers.
        let (db, root) = fixture(&[
            ("src/a.ts", "export function serverEntry() { return 1; }\n"),
            (
                "src/b.ts",
                "import { serverEntry } from './a';\nfunction runInit() { return serverEntry(); }\n",
            ),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let r = grep(&db, id, &root, "serverEntry").unwrap();

        assert!(
            r.total_hits >= 3,
            "definition + import + call, got {}",
            r.total_hits
        );
        let paths: Vec<_> = r.groups.iter().map(|g| g.path.as_str()).collect();
        assert!(
            paths.contains(&"src/b.ts"),
            "must reach the calling file: {paths:?}"
        );
        assert!(
            r.groups
                .iter()
                .any(|g| g.symbol.as_deref() == Some("runInit")),
            "call site must be grouped under its enclosing symbol"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_match_outside_any_definition_is_still_reported() {
        // Top-level imports sit in no symbol's span. Dropping them would make an
        // exhaustive search quietly non-exhaustive.
        let (db, root) = fixture(&[("src/a.ts", "import { x } from './b';\nfunction f() {}\n")]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let r = grep(&db, id, &root, "import").unwrap();
        assert_eq!(r.total_hits, 1);
        assert_eq!(
            r.groups[0].symbol, None,
            "file-level hit, not attributed to f()"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_keeps_same_named_symbols_as_separate_groups() {
        let (db, root) = fixture(&[(
            "src/a.ts",
            "class A {\n  run() {\n    needle();\n  }\n}\nclass B {\n  run() {\n    needle();\n  }\n}\n",
        )]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let r = grep(&db, id, &root, "needle").unwrap();
        let runs: Vec<_> = r
            .groups
            .iter()
            .filter(|g| g.symbol.as_deref() == Some("run"))
            .collect();
        assert_eq!(
            runs.len(),
            2,
            "same name, distinct symbol ids/spans: {}",
            r.groups.len()
        );
        assert_ne!(runs[0].start_line, runs[1].start_line);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn grep_regex_fixed_case_and_scope_options_are_distinct() {
        let (db, root) = fixture(&[
            ("src/a.ts", "const a = 'A.B';\nconst b = 'axb';\n"),
            ("src-extra/a.ts", "const c = 'a.b';\n"),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let regex = grep_with_options(
            &db,
            id,
            &root,
            "a.b",
            GrepOptions {
                ignore_case: true,
                fixed: false,
                scope: Some("src"),
            },
        )
        .unwrap();
        assert_eq!(regex.total_hits, 2, "regex dot also matches axb");
        let fixed = grep_with_options(
            &db,
            id,
            &root,
            "a.b",
            GrepOptions {
                ignore_case: true,
                fixed: true,
                scope: Some("src"),
            },
        )
        .unwrap();
        assert_eq!(fixed.total_hits, 1);
        assert!(fixed.groups.iter().all(|group| group.path == "src/a.ts"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_extractor_rows_force_a_rebuild() {
        let (db, root) = fixture(&[("src/a.ts", "function f() {}\n")]);
        db.execute(
            "update repos set extractor_stamp='obsolete-extractor' where root=?1",
            [root.to_string_lossy()],
        )
        .unwrap();
        assert!(repo_id_of(&db, &root).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn python_imports_resolve_to_internal_file_nodes() {
        let (db, root) = fixture(&[
            (
                "pkg/a.py",
                "from pkg.b import helper\ndef run():\n    helper()\n",
            ),
            ("pkg/b.py", "def helper():\n    pass\n"),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let imports: i64 = db
            .query_row(
                "select count(*) from edges e
                   join symbols src on src.id=e.src_symbol_id
                   join symbols dst on dst.id=e.dst_symbol_id
                  where e.repo_id=?1 and e.kind='imports'
                    and src.name='pkg/a.py' and dst.name='pkg/b.py'",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(imports, 1);
        let (_, reached) = callers(&db, id, "pkg/b.py", false, 1).unwrap();
        assert!(
            reached
                .iter()
                .any(|hit| hit.name == "pkg/a.py" && hit.edge == "imports")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn go_package_imports_and_receiver_calls_resolve() {
        let (db, root) = fixture(&[
            ("go.mod", "module example.com/project\n"),
            (
                "main.go",
                "package main\nimport _ \"example.com/project/pkg\"\nfunc main() {}\n",
            ),
            (
                "pkg/worker.go",
                "package pkg\ntype Worker struct{}\nfunc (w *Worker) Step() {}\nfunc (w *Worker) Run() { w.Step() }\n",
            ),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let imports: i64 = db
            .query_row(
                "select count(*) from edges e
                   join symbols src on src.id=e.src_symbol_id
                   join symbols dst on dst.id=e.dst_symbol_id
                  where e.repo_id=?1 and e.kind='imports'
                    and src.name='main.go' and dst.name='pkg/worker.go'",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(imports, 1);
        let calls: i64 = db
            .query_row(
                "select count(*) from edges e
                   join symbols src on src.id=e.src_symbol_id
                   join symbols dst on dst.id=e.dst_symbol_id
                  where e.repo_id=?1 and e.kind='calls'
                    and src.name='Run' and dst.name='Step'",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(calls, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rust_module_imports_and_calls_resolve() {
        let (db, root) = fixture(&[
            ("rust/src/db.rs", "pub fn helper() {}\n"),
            (
                "rust/src/main.rs",
                "mod db;\nuse crate::db::helper;\nfn main() { helper(); }\n",
            ),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let imports: i64 = db
            .query_row(
                "select count(*) from edges e
                   join symbols src on src.id=e.src_symbol_id
                   join symbols dst on dst.id=e.dst_symbol_id
                  where e.repo_id=?1 and e.kind='imports'
                    and src.name='rust/src/main.rs' and dst.name='rust/src/db.rs'",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(imports, 1, "mod/use imports dedupe to one file edge");
        let calls: i64 = db
            .query_row(
                "select count(*) from edges e
                   join symbols src on src.id=e.src_symbol_id
                   join symbols dst on dst.id=e.dst_symbol_id
                  where e.repo_id=?1 and e.kind='calls'
                    and src.name='main' and dst.name='helper'",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(calls, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn automation_edges(db: &Connection) -> Vec<(String, String, String, String)> {
        db.prepare(
            "select sf.path, src.name, dst.name, df.path from edges e
            join symbols src on src.id=e.src_symbol_id join files sf on sf.id=src.file_id
            join symbols dst on dst.id=e.dst_symbol_id join files df on df.id=dst.file_id
            where e.kind in ('calls', 'imports') order by sf.path,src.name,dst.name,df.path",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    #[test]
    fn ansible_imports_roles_handlers_and_cache_refresh() {
        let (mut db, root) = fixture(&[
            (
                "site.yml",
                r#"
- name: Configure web
  hosts: all
  roles: [web]
  vars_files: [vars/common.yml]
  tasks:
    - name: Load more
      ansible.builtin.import_tasks: {file: tasks/more.yml}
    - name: Change config
      copy: {src: a, dest: b}
      notify: [restart, reload services]
    - name: Nested
      block:
        - name: Load vars
          include_vars: vars/extra.yaml
      rescue:
        - name: Recover
          import_role: {name: recovery, tasks_from: repair}
  handlers:
    - name: Local listener
      debug: {msg: done}
      listen: reload services
"#,
            ),
            ("vars/common.yml", "port: 80\n"),
            ("vars/extra.yaml", "enabled: true\n"),
            (
                "tasks/more.yml",
                "- name: Included task\n  debug: {msg: changed}\n  notify: restart\n",
            ),
            (
                "roles/web/tasks/main.yml",
                "- name: Setup\n  import_tasks: install.yml\n",
            ),
            (
                "roles/web/tasks/install.yml",
                "- name: Install\n  package: {name: nginx}\n",
            ),
            (
                "roles/web/handlers/main.yml",
                "- name: restart\n  service: {name: nginx, state: restarted}\n  listen: reload services\n",
            ),
            (
                "roles/web/meta/main.yml",
                "dependencies: [{role: common}]\n",
            ),
            (
                "roles/common/tasks/main.yaml",
                "- name: Common\n  debug: {msg: ready}\n",
            ),
            (
                "roles/recovery/tasks/repair.yml",
                "- name: Repair\n  debug: {msg: ready}\n",
            ),
            (
                "roles/recovery/tasks/main.yml",
                "- debug: {msg: should not load}\n",
            ),
        ]);
        let edges = automation_edges(&db);
        let has = |source: &str, target: &str| {
            edges.iter().any(|(_, s, d, _)| s == source && d == target)
        };
        assert!(
            has("play: Configure web", "roles/web/tasks/main.yml"),
            "{edges:?}"
        );
        assert!(has("play: Configure web", "vars/common.yml"));
        assert!(has("task: Load more", "tasks/more.yml"));
        assert!(has("task: Load vars", "vars/extra.yaml"));
        assert!(has("task: Setup", "roles/web/tasks/install.yml"));
        assert!(has(
            "roles/web/meta/main.yml",
            "roles/common/tasks/main.yaml"
        ));
        assert!(has("task: Recover", "roles/recovery/tasks/repair.yml"));
        assert!(!has("task: Recover", "roles/recovery/tasks/main.yml"));
        assert!(has("task: Change config", "handler: restart"));
        assert!(has("task: Change config", "handler: Local listener"));
        assert!(has("task: Included task", "handler: restart"));
        let repo = repo_id_of(&db, &root).unwrap().unwrap();
        let (_, reached) = callers(&db, repo, "play: Configure web", true, 8).unwrap();
        assert!(reached.iter().any(|r| r.name == "handler: restart"));
        let stats = build(&mut db, &root).unwrap();
        assert_eq!(stats.parsed, 0);
        assert_eq!(automation_edges(&db), edges);
        std::fs::write(
            root.join("roles/web/handlers/main.yml"),
            "- name: renamed\n  debug: {msg: done}\n",
        )
        .unwrap();
        assert_eq!(build(&mut db, &root).unwrap().parsed, 1);
        assert!(
            !automation_edges(&db)
                .iter()
                .any(|(_, _, dst, _)| dst == "handler: restart")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ansible_handler_names_are_scoped_to_plays_and_reachable_roles() {
        let (db, root) = fixture(&[
            (
                "site.yml",
                r#"
- hosts: web
  roles: [web]
  tasks:
    - name: Web change
      debug: {msg: changed}
      notify: restart
- hosts: db
  roles: [database]
  tasks:
    - name: DB change
      debug: {msg: changed}
      notify: restart
- hosts: empty
  tasks:
    - name: No handler
      debug: {msg: changed}
      notify: restart
"#,
            ),
            (
                "roles/web/handlers/main.yml",
                "- name: restart\n  debug: {msg: web}\n",
            ),
            (
                "roles/database/handlers/main.yml",
                "- name: restart\n  debug: {msg: db}\n",
            ),
            (
                "unrelated.yml",
                "- hosts: all\n  handlers:\n    - name: restart\n      debug: {msg: unrelated}\n",
            ),
        ]);
        let edges = automation_edges(&db);
        let targets = |task: &str| {
            edges
                .iter()
                .filter(|(_, s, d, _)| s == task && d == "handler: restart")
                .map(|(_, _, _, p)| p.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(targets("task: Web change"), ["roles/web/handlers/main.yml"]);
        assert_eq!(
            targets("task: DB change"),
            ["roles/database/handlers/main.yml"]
        );
        assert!(targets("task: No handler").is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ansible_static_paths_are_local_and_dynamic_targets_are_not_guessed() {
        let (db, root) = fixture(&[
            (
                "playbooks/site.yaml",
                r#"
- ansible.builtin.import_playbook: child.yml
- hosts: all
  roles: [web, acme.demo.tools]
  tasks:
    - name: Dynamic
      import_tasks: '{{ chosen }}.yml'
    - name: Escape
      import_tasks: ../../escape.yml
    - name: Foreign module
      example.custom.import_tasks: child.yml
"#,
            ),
            ("playbooks/child.yml", "- hosts: all\n  tasks: []\n"),
            ("escape.yml", "- debug: {msg: unrelated}\n"),
            (
                "playbooks/roles/web/tasks/main.yml",
                "- debug: {msg: local}\n",
            ),
            ("roles/web/tasks/main.yml", "- debug: {msg: root}\n"),
            (
                "collections/ansible_collections/acme/demo/roles/tools/tasks/main.yaml",
                "- debug: {msg: collection}\n",
            ),
            (
                "config.yml",
                "name: app\nnotify: restart\nroles: [web]\ninclude_tasks: escape.yml\n",
            ),
        ]);
        let edges = automation_edges(&db);
        assert!(
            edges
                .iter()
                .any(|(_, s, d, _)| s == "playbooks/site.yaml" && d == "playbooks/child.yml")
        );
        assert!(
            edges
                .iter()
                .any(|(_, s, d, _)| s == "play: all" && d == "playbooks/roles/web/tasks/main.yml")
        );
        assert!(
            !edges
                .iter()
                .any(|(_, s, d, _)| s == "play: all" && d == "roles/web/tasks/main.yml")
        );
        assert!(edges.iter().any(|(_, _, d, _)| d
            == "collections/ansible_collections/acme/demo/roles/tools/tasks/main.yaml"));
        assert!(!edges.iter().any(|(p, s, _, _)| p == "config.yml"
            || ["task: Dynamic", "task: Escape", "task: Foreign module"].contains(&s.as_str())));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ansible_standalone_roles_listen_fanout_and_duplicate_names() {
        let (db, root) = fixture(&[
            (
                "tasks/main.yml",
                "- name: Change\n  debug: {msg: changed}\n  notify: [restart, topic]\n",
            ),
            (
                "handlers/main.yml",
                "- name: restart\n  debug: {msg: one}\n- name: restart\n  debug: {msg: two}\n- name: listener one\n  listen: topic\n  debug: {msg: one}\n- name: listener two\n  listen: [topic]\n  debug: {msg: two}\n",
            ),
        ]);
        let edges = automation_edges(&db);
        assert!(
            !edges
                .iter()
                .any(|(_, s, d, _)| s == "task: Change" && d == "handler: restart")
        );
        for listener in ["handler: listener one", "handler: listener two"] {
            assert!(
                edges
                    .iter()
                    .any(|(_, s, d, _)| s == "task: Change" && d == listener),
                "{edges:?}"
            );
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn actions_composites_reusable_workflows_and_outputs_are_traversable() {
        let (mut db, root) = fixture(&[
            (
                ".github/workflows/ci.yml",
                r#"
name: CI
on:
  workflow_dispatch:
    inputs:
      target: {type: string}
jobs:
  build:
    outputs:
      version: ${{ steps.package.outputs.version }}
    steps:
      - id: package
        uses: ./.github/actions/package
        with:
          target: ${{ inputs.target }}
      - id: consume
        if: steps.package.outcome == 'success'
        run: echo '${{ steps.package.outputs.version }}'
  deploy:
    needs: [build]
    uses: ./.github/workflows/deploy.yaml
    with:
      version: ${{ needs.build.outputs.version }}
  remote:
    needs: deploy
    uses: example/ci/.github/workflows/publish.yml@v2
    with:
      published: ${{ needs.deploy.outputs.published }}
"#,
            ),
            (
                ".github/workflows/deploy.yaml",
                r#"
on:
  workflow_call:
    inputs:
      version: {type: string}
    outputs:
      published:
        value: ${{ jobs.publish.outputs.result }}
jobs:
  publish:
    outputs:
      result: ${{ steps.deploy.outputs.result }}
    steps:
      - id: deploy
        uses: docker://alpine:3.22
        with: {version: '${{ inputs.version }}'}
"#,
            ),
            (
                ".github/actions/package/action.yml",
                r#"
name: Package
inputs:
  target: {description: Target}
outputs:
  version:
    value: ${{ steps.build.outputs.version }}
runs:
  using: composite
  steps:
    - id: setup
      uses: ./.github/actions/setup
    - id: build
      if: steps.setup.outcome == 'success'
      run: echo '${{ inputs.target }}'
      shell: bash
    - uses: actions/upload-artifact@v4
"#,
            ),
            (
                ".github/actions/setup/action.yaml",
                "name: Setup\nruns:\n  using: composite\n  steps:\n    - id: checkout\n      uses: actions/checkout@v7\n",
            ),
        ]);
        let edges = automation_edges(&db);
        let has = |s: &str, d: &str| {
            edges
                .iter()
                .any(|(_, source, target, _)| source == s && target == d)
        };
        assert!(has("job: deploy", "job: build"), "{edges:?}");
        assert!(has("job: remote", "job: deploy"));
        assert!(has("job: deploy", "output: job.build.version"));
        assert!(has("output: job.build.version", "step: build.package"));
        assert!(has("step: build.consume", "step: build.package"));
        assert!(has("step: build.package", "input.target"));
        assert!(has(
            "step: build.package",
            ".github/actions/package/action.yml"
        ));
        assert!(has("step: setup", ".github/actions/setup/action.yaml"));
        assert!(has("step: build", "step: setup"));
        assert!(has("step: build", "input.target"));
        assert!(has("output: action.version", "step: build"));
        assert!(has(
            "output: workflow.published",
            "output: job.publish.result"
        ));
        assert!(has("job: deploy", ".github/workflows/deploy.yaml"));
        assert!(has(
            "job: remote",
            "example/ci/.github/workflows/publish.yml@v2"
        ));
        assert!(has("step: publish.deploy", "docker://alpine:3.22"));
        let repo = repo_id_of(&db, &root).unwrap().unwrap();
        let (seeds, reached) = callers(&db, repo, "step: build.package", true, 8).unwrap();
        assert_eq!(seeds.len(), 1, "literal dotted symbol lookup");
        assert!(
            reached.iter().any(|r| r.name == "actions/checkout@v7"),
            "{reached:?}"
        );
        let (_, incoming) = callers(&db, repo, "actions/checkout@v7", false, 8).unwrap();
        assert!(incoming.iter().any(|r| r.name == "job: build"));
        assert_eq!(build(&mut db, &root).unwrap().parsed, 0);
        assert_eq!(automation_edges(&db), edges);
        std::fs::write(root.join(".github/actions/setup/action.yaml"), "name: Setup\nruns:\n  using: composite\n  steps:\n    - id: checkout\n      uses: actions/checkout@v8\n").unwrap();
        assert_eq!(build(&mut db, &root).unwrap().parsed, 1);
        let edges = automation_edges(&db);
        assert!(!edges.iter().any(|(_, _, d, _)| d == "actions/checkout@v7"));
        assert!(edges.iter().any(|(_, _, d, _)| d == "actions/checkout@v8"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn actions_references_are_scoped_and_ignore_literals_comments_and_dynamic_indexes() {
        let (db, root) = fixture(&[
            (
                ".github/workflows/ci.yml",
                r#"
jobs:
  build:
    steps:
      - id: setup
        run: echo ready
      - id: phantom
        run: echo present
      - id: consume
        if: steps['setup'].outcome == 'success'
        run: |
          echo '${{ steps.setup.outputs.x }}'
          echo "${{ format('steps.phantom.outputs.x needs.ghost.result') }}"
          echo '${{ steps[inputs.target].outputs.x }}'
          echo 'steps.phantom.outputs.x'
        # ${{ steps.phantom.outputs.x }}
        with:
          if: steps.phantom.outputs.x
  other:
    steps:
      - id: setup
        run: echo ready
      - id: consume
        if: steps.setup.outcome == 'success'
  last:
    steps:
      - id: consume
        if: steps.setup.outcome == 'success'
"#,
            ),
            (
                ".github/workflows/unrelated.yml",
                "jobs:\n  build:\n    steps:\n      - id: setup\n        run: echo unrelated\n",
            ),
            (
                "config.yml",
                "jobs:\n  build:\n    needs: other\nuses: ./.github/actions/setup\n",
            ),
            (
                ".github/actions/setup/action.yml",
                "name: Setup\nruns: {using: composite, steps: []}\n",
            ),
        ]);
        let edges = automation_edges(&db);
        let calls: Vec<_> = edges
            .iter()
            .filter(|(_, s, _, _)| s == "step: build.consume")
            .collect();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].2, "step: build.setup");
        assert_eq!(calls[0].3, ".github/workflows/ci.yml");
        assert!(
            edges
                .iter()
                .any(|(_, s, d, _)| s == "step: other.consume" && d == "step: other.setup")
        );
        assert!(
            !edges
                .iter()
                .any(|(p, s, _, _)| p == "config.yml" || s == "step: last.consume")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn actions_root_actions_js_entrypoints_and_missing_local_targets() {
        let (db, root) = fixture(&[
            (
                ".github/workflows/ci.yml",
                r#"
jobs:
  build:
    steps:
      - id: root
        uses: ./
      - id: wrong
        uses: ./scripts/main.js
      - id: dynamic
        uses: ${{ inputs.action }}
      - id: escape
        uses: ./../../outside
  invalid:
    uses: ./scripts/main.js
"#,
            ),
            (
                "action.yaml",
                "name: Root\nruns: {using: node24, main: scripts/main.js, post: scripts/post.js}\n",
            ),
            ("scripts/main.js", "export function main() {}\n"),
            ("scripts/post.js", "export function post() {}\n"),
        ]);
        let edges = automation_edges(&db);
        assert!(
            edges
                .iter()
                .any(|(_, s, d, _)| s == "step: build.root" && d == "action.yaml"),
            "{edges:?}"
        );
        assert!(
            edges
                .iter()
                .any(|(_, s, d, _)| s == "action.yaml" && d == "scripts/main.js")
        );
        assert!(
            edges
                .iter()
                .any(|(_, s, d, _)| s == "action.yaml" && d == "scripts/post.js")
        );
        assert!(!edges.iter().any(|(_, s, _, _)| {
            [
                "step: build.wrong",
                "step: build.dynamic",
                "step: build.escape",
                "job: invalid",
            ]
            .contains(&s.as_str())
        }));
        let repo = repo_id_of(&db, &root).unwrap().unwrap();
        let (seeds, _) = callers(&db, repo, "jobs.build.steps.uses", false, 1).unwrap();
        assert_eq!(seeds.len(), 4, "existing YAML dotted keys remain queryable");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn actions_reusable_output_consumers_and_ambiguous_steps() {
        let (db, root) = fixture(&[
            (
                ".github/workflows/ci.yml",
                r#"
jobs:
  producer:
    uses: example/ci/.github/workflows/build.yml@v1
  consumer:
    needs: producer
    steps:
      - id: consume
        run: echo '${{ needs.producer.outputs.version }}'
      - id: duplicate
        run: echo one
      - id: duplicate
        run: echo two
      - id: ambiguous
        if: steps.duplicate.outcome == 'success'
        run: echo unknown
"#,
            ),
            (
                "action.yml",
                "- hosts: all\n  tasks:\n    - name: Ansible action\n      debug: {msg: ready}\n",
            ),
        ]);
        let edges = automation_edges(&db);
        assert!(
            edges
                .iter()
                .any(|(_, s, d, _)| s == "step: consumer.consume" && d == "job: producer")
        );
        assert!(
            !edges
                .iter()
                .any(|(_, s, _, _)| s == "step: consumer.ambiguous")
        );
        assert!(edges.iter().any(|(p, s, d, _)| p == "action.yml"
            && s == "play: all"
            && d == "task: Ansible action"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compose_services_resources_includes_and_extends_are_scoped() {
        let (mut db, root) = fixture(&[
            (
                "compose.yaml",
                r#"
include: [shared/compose.yml]
services:
  web:
    image: nginx
    depends_on: {db: {condition: service_healthy}}
    networks: [frontend]
    volumes: [data:/var/data, './bind:/data', '/anonymous']
    configs: [{source: settings, target: /settings}]
    secrets: [password]
    extends: {file: base/compose.yml, service: base}
  db:
    image: postgres
    volumes: [{type: volume, source: data, target: /var/db}]
  sidecar:
    image: busybox
    network_mode: service:web
    volumes_from: [db:ro]
    links: [cache:redis]
networks:
  frontend:
volumes:
  data:
configs:
  settings: {file: config.yml}
secrets:
  password: {external: true}
"#,
            ),
            ("shared/compose.yml", "services:\n  cache: {image: redis}\n"),
            ("base/compose.yml", "services:\n  base: {image: alpine}\n"),
            ("other/compose.yml", "services:\n  db: {image: unrelated}\n"),
            ("config.yml", "port: 8080\n"),
        ]);
        let edges = automation_edges(&db);
        let has = |s: &str, d: &str| edges.iter().any(|(_, a, b, _)| a == s && b == d);
        for target in [
            "service: db",
            "network: frontend",
            "volume: data",
            "config: settings",
            "secret: password",
            "service: base",
        ] {
            assert!(has("service: web", target), "missing {target}: {edges:?}");
        }
        assert!(has("service: db", "volume: data"));
        assert!(has("service: sidecar", "service: web"));
        assert!(has("service: sidecar", "service: db"));
        assert!(has("service: sidecar", "service: cache"));
        assert!(has("config: settings", "config.yml"));
        assert!(
            !edges
                .iter()
                .any(|(p, _, _, target)| p == "compose.yaml" && target == "other/compose.yml")
        );
        assert_eq!(build(&mut db, &root).unwrap().parsed, 0);
        assert_eq!(automation_edges(&db), edges);
        std::fs::write(
            root.join("shared/compose.yml"),
            "services:\n  renamed: {image: redis}\n",
        )
        .unwrap();
        assert_eq!(build(&mut db, &root).unwrap().parsed, 1);
        assert!(
            !automation_edges(&db)
                .iter()
                .any(|(_, _, d, _)| d == "service: cache")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compose_ambiguous_and_interpolated_targets_are_not_guessed() {
        let (db, root) = fixture(&[
            (
                "compose.yml",
                r#"
include:
  - path: [one.yml, two.yml]
services:
  app:
    image: app
    depends_on: [db, '${DATABASE}']
    volumes: ['./data:/data', '${VOLUME}:/var/lib', '/tmp:/tmp']
    extends: {service: '${BASE}'}
"#,
            ),
            ("one.yml", "services:\n  db: {image: one}\n"),
            ("two.yml", "services:\n  db: {image: two}\n"),
            ("unrelated.yml", "services: [app, db]\n"),
        ]);
        assert!(
            !automation_edges(&db)
                .iter()
                .any(|(_, s, _, _)| s == "service: app")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn inherited_yaml_relationships_reach_compose_ansible_and_actions_consumers() {
        let (db, root) = fixture(&[
            (
                "compose.yml",
                "x-defaults: &defaults {image: nginx, depends_on: [db]}\nservices:\n  web: {<<: *defaults}\n  independent: {<<: *defaults, depends_on: []}\n  db: {image: postgres}\n",
            ),
            (
                "play.yml",
                "- hosts: all\n  vars:\n    common: &common {debug: {msg: changed}, notify: restart}\n  tasks:\n    - {<<: *common, name: Change}\n  handlers:\n    - {name: restart, debug: {msg: done}}\n",
            ),
            (
                ".github/workflows/ci.yml",
                "jobs:\n  first:\n    steps:\n      - &checkout {id: checkout, uses: actions/checkout@v7}\n  second:\n    steps:\n      - *checkout\n",
            ),
        ]);
        let edges = automation_edges(&db);
        let has = |s: &str, d: &str| edges.iter().any(|(_, a, b, _)| a == s && b == d);
        assert!(has("service: web", "service: db"));
        assert!(!has("service: independent", "service: db"));
        assert!(has("task: Change", "handler: restart"));
        assert!(has("step: second.checkout", "actions/checkout@v7"));
        assert!(
            has("services.web.<<", "defaults"),
            "anchor provenance remains traceable"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn kubernetes_workloads_configuration_services_ingress_and_rbac() {
        let (mut db, root) = fixture(&[
            (
                "app.yml",
                r#"
apiVersion: apps/v1
kind: Deployment
metadata: {name: web, namespace: prod}
spec:
  template:
    metadata: {labels: {app: web}}
    spec:
      serviceAccountName: runner
      imagePullSecrets: [{name: auth}]
      containers:
        - name: web
          image: nginx
          envFrom: [{configMapRef: {name: settings}}]
          env: [{name: PASS, valueFrom: {secretKeyRef: {name: auth, key: password}}}]
      volumes:
        - name: data
          persistentVolumeClaim: {claimName: data}
        - name: config
          projected: {sources: [{configMap: {name: settings}}, {secret: {name: auth}}]}
---
apiVersion: v1
kind: Service
metadata: {name: web, namespace: prod}
spec: {selector: {app: web}}
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: web, namespace: prod}
spec:
  defaultBackend: {service: {name: web, port: {number: 80}}}
  tls: [{secretName: auth}]
"#,
            ),
            (
                "resources.yml",
                r#"
apiVersion: v1
kind: List
items:
  - {apiVersion: v1, kind: ConfigMap, metadata: {name: settings, namespace: prod}}
  - {apiVersion: v1, kind: Secret, metadata: {name: auth, namespace: prod}}
  - {apiVersion: v1, kind: PersistentVolumeClaim, metadata: {name: data, namespace: prod}}
  - {apiVersion: v1, kind: ServiceAccount, metadata: {name: runner, namespace: prod}}
  - {apiVersion: rbac.authorization.k8s.io/v1, kind: ClusterRole, metadata: {name: reader}}
  - apiVersion: rbac.authorization.k8s.io/v1
    kind: RoleBinding
    metadata: {name: read, namespace: prod}
    roleRef: {apiGroup: rbac.authorization.k8s.io, kind: ClusterRole, name: reader}
    subjects: [{kind: ServiceAccount, name: runner, namespace: prod}]
"#,
            ),
            (
                "dev.yml",
                "apiVersion: apps/v1\nkind: Deployment\nmetadata: {name: web, namespace: dev}\nspec: {template: {metadata: {labels: {app: web}}, spec: {containers: []}}}\n",
            ),
        ]);
        let edges = automation_edges(&db);
        let has = |s: &str, d: &str| edges.iter().any(|(_, a, b, _)| a == s && b == d);
        for target in [
            "ConfigMap: prod/settings",
            "Secret: prod/auth",
            "PersistentVolumeClaim: prod/data",
            "ServiceAccount: prod/runner",
        ] {
            assert!(
                has("Deployment: prod/web", target),
                "missing {target}: {edges:?}"
            );
        }
        assert!(has("Service: prod/web", "Deployment: prod/web"));
        assert!(!has("Service: prod/web", "Deployment: dev/web"));
        assert!(has("Ingress: prod/web", "Service: prod/web"));
        assert!(has("Ingress: prod/web", "Secret: prod/auth"));
        assert!(has("RoleBinding: prod/read", "ClusterRole: reader"));
        assert!(has("RoleBinding: prod/read", "ServiceAccount: prod/runner"));
        assert_eq!(build(&mut db, &root).unwrap().parsed, 0);
        assert_eq!(automation_edges(&db), edges);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn kubernetes_missing_duplicate_and_dynamic_namespaces_do_not_guess() {
        let (db, root) = fixture(&[
            (
                "pod.yml",
                "apiVersion: v1\nkind: Pod\nmetadata: {name: consumer}\nspec: {containers: [{name: app, envFrom: [{configMapRef: {name: settings}}]}]}\n",
            ),
            (
                "one.yml",
                "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: settings}\n",
            ),
            (
                "two.yml",
                "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: settings}\n",
            ),
            (
                "custom.yml",
                "apiVersion: custom.example/v1\nkind: ConfigMap\nmetadata: {name: settings}\n",
            ),
            (
                "dynamic.yml",
                "apiVersion: v1\nkind: Pod\nmetadata: {name: dynamic, namespace: '{{ namespace }}'}\nspec: {serviceAccountName: runner}\n",
            ),
        ]);
        assert!(
            !automation_edges(&db)
                .iter()
                .any(|(_, s, _, _)| s == "Pod: default/consumer" || s == "Pod: default/dynamic")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn kustomize_overlays_components_and_patches_follow_only_reachable_sources() {
        let (mut db, root) = fixture(&[
            ("base/Kustomization", "resources: [app.yml, service.yml]\n"),
            (
                "base/app.yml",
                "apiVersion: apps/v1\nkind: Deployment\nmetadata: {name: web, labels: {app: web}}\nspec: {template: {metadata: {labels: {app: web}}, spec: {containers: []}}}\n",
            ),
            (
                "base/service.yml",
                "apiVersion: v1\nkind: Service\nmetadata: {name: web}\nspec: {selector: {app: web}}\n",
            ),
            (
                "component/kustomization.yaml",
                "apiVersion: kustomize.config.k8s.io/v1alpha1\nkind: Component\nresources: [config.yml]\n",
            ),
            (
                "component/config.yml",
                "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: settings}\n",
            ),
            (
                "overlays/prod/kustomization.yml",
                r#"
resources: [../../base]
components: [../../component]
patches:
  - path: replicas.yml
    target: {group: apps, kind: Deployment, name: 'web.*', labelSelector: 'app=web'}
  - patch: '[{"op":"add","path":"/metadata/labels/prod","value":"true"}]'
    target: {kind: ConfigMap, name: settings}
configMapGenerator:
  - name: generated
    files: [settings=values.yml]
"#,
            ),
            (
                "overlays/prod/replicas.yml",
                "apiVersion: apps/v1\nkind: Deployment\nmetadata: {name: web}\nspec: {replicas: 3}\n",
            ),
            ("overlays/prod/values.yml", "feature: enabled\n"),
            (
                "other/app.yml",
                "apiVersion: apps/v1\nkind: Deployment\nmetadata: {name: web, namespace: other, labels: {app: web}}\n",
            ),
        ]);
        let edges = automation_edges(&db);
        let has = |s: &str, d: &str| edges.iter().any(|(_, a, b, _)| a == s && b == d);
        assert!(has("overlays/prod/kustomization.yml", "base/Kustomization"));
        assert!(has("patch: patches#1", "overlays/prod/replicas.yml"));
        assert!(has("patch: patches#1", "Deployment: default/web"));
        assert!(!has("patch: patches#1", "Deployment: other/web"));
        assert!(has("patch: patches#2", "ConfigMap: default/settings"));
        assert!(has("generator: generated", "overlays/prod/values.yml"));
        let services: Vec<_> = edges
            .iter()
            .filter(|(_, s, d, _)| s == "Service: default/web" && d == "Deployment: default/web")
            .collect();
        assert_eq!(
            services.len(),
            1,
            "patch metadata must not duplicate a deployed resource"
        );
        assert_eq!(services[0].3, "base/app.yml");
        assert_eq!(build(&mut db, &root).unwrap().parsed, 0);
        assert_eq!(automation_edges(&db), edges);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn kustomize_strategic_patch_metadata_and_unsupported_selectors() {
        let (db, root) = fixture(&[
            (
                "kustomization.yaml",
                "resources: [app.yml]\npatchesStrategicMerge: [patch.yml]\npatches:\n  - path: patch.yml\n    target: {kind: ConfigMap, labelSelector: 'app in (web, api)'}\n  - path: patch.yml\n    target: {kind: ConfigMap, annotationSelector: enabled}\n",
            ),
            (
                "app.yml",
                "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: config, labels: {app: web}}\n",
            ),
            (
                "patch.yml",
                "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: config}\ndata: {key: value}\n",
            ),
        ]);
        let edges = automation_edges(&db);
        assert!(
            edges
                .iter()
                .any(|(_, s, d, p)| s == "patch: patchesStrategicMerge#1"
                    && d == "ConfigMap: default/config"
                    && p == "app.yml")
        );
        assert!(!edges.iter().any(
            |(_, s, d, _)| s.starts_with("patch: patches#") && d == "ConfigMap: default/config"
        ));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn gitlab_includes_templates_needs_artifacts_and_child_pipeline_scopes() {
        let (mut db, root) = fixture(&[
            (
                ".gitlab-ci.yml",
                r#"
include: [{local: ci/jobs.yml}, {local: ci/templates.yml}]
stages: [build, test]
consumer:
  stage: test
  needs: [{job: build, artifacts: true}]
  dependencies: [build]
  script: echo test
child:
  trigger: {include: [{local: ci/child.yml}]}
"#,
            ),
            (
                "ci/jobs.yml",
                "build:\n  stage: build\n  extends: .base\n  script:\n    - !reference [.commands, script]\n",
            ),
            (
                "ci/templates.yml",
                ".base: {image: alpine}\n.commands: {script: echo build}\ndefault: {before_script: echo prepare}\n",
            ),
            ("ci/child.yml", "build: {script: echo child}\n"),
            ("other.yml", "build: {script: echo unrelated}\n"),
        ]);
        let edges = automation_edges(&db);
        let has = |s: &str, d: &str| edges.iter().any(|(_, a, b, _)| a == s && b == d);
        assert!(has("gitlab job: build", "gitlab job: .base"), "{edges:?}");
        assert!(has("gitlab job: build", "gitlab job: .commands"));
        assert!(has("gitlab job: build", "gitlab stage: build"));
        assert!(has("gitlab job: consumer", "gitlab default"));
        assert!(has("gitlab job: child", "ci/child.yml"));
        let needs: Vec<_> = edges
            .iter()
            .filter(|(_, s, d, _)| s == "gitlab job: consumer" && d == "gitlab job: build")
            .collect();
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].3, "ci/jobs.yml");
        assert!(
            !edges
                .iter()
                .any(|(p, _, d, _)| p == "ci/child.yml" && d == "gitlab default")
        );
        assert_eq!(build(&mut db, &root).unwrap().parsed, 0);
        assert_eq!(automation_edges(&db), edges);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn gitlab_remote_includes_duplicates_and_runtime_targets_are_conservative() {
        let (db, root) = fixture(&[
            (
                ".gitlab-ci.yml",
                r#"
include:
  - local: one.yml
  - local: two.yml
  - project: team/templates
    ref: v1
    file: [ci.yml]
  - remote: https://example.org/ci.yml
consumer:
  script: echo test
  needs: [build, '$TARGET', {job: build, project: another, ref: main}]
  inherit: {default: false}
  extends: '$TEMPLATE'
"#,
            ),
            (
                "one.yml",
                "build: {script: echo one}\ndefault: {image: alpine}\n",
            ),
            ("two.yml", "build: {script: echo two}\n"),
            ("plain.yml", "include: https://example.org/not-ci.yml\n"),
        ]);
        let edges = automation_edges(&db);
        assert!(edges.iter().any(|(p,_,d,_)|p==".gitlab-ci.yml"&&d=="gitlab:team/templates@v1:ci.yml"));
        assert!(
            edges.iter().any(|(p, _, d, _)| p == ".gitlab-ci.yml"
                && d == "gitlab:remote:https://example.org/ci.yml")
        );
        assert!(
            !edges
                .iter()
                .any(|(p, s, _, _)| p == "plain.yml" || s == "gitlab job: consumer")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shell_and_yaml_relationships_resolve_across_indexed_files() {
        let (db, root) = fixture(&[
            (
                ".github/workflows/ci.yml",
                "defaults: &linux\n  runs-on: ubuntu-latest\njobs:\n  build:\n    <<: *linux\n    steps:\n      - uses: ./.github/actions/setup\n",
            ),
            (
                ".github/actions/setup/action.yml",
                "name: Setup\nruns:\n  using: composite\n",
            ),
            (
                "scripts/main.sh",
                "source \"./lib.sh\"\nmain() { helper; }\n",
            ),
            ("scripts/lib.sh", "helper() { echo ready; }\n"),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();

        let edge_count = |kind: &str, source: &str, target: &str| -> i64 {
            db.query_row(
                "select count(*) from edges e
                   join symbols src on src.id=e.src_symbol_id
                   join symbols dst on dst.id=e.dst_symbol_id
                  where e.repo_id=?1 and e.kind=?2
                    and src.name=?3 and dst.name=?4",
                rusqlite::params![id, kind, source, target],
                |row| row.get(0),
            )
            .unwrap()
        };

        assert_eq!(
            edge_count(
                "imports",
                ".github/workflows/ci.yml",
                ".github/actions/setup/action.yml"
            ),
            1
        );
        assert_eq!(
            edge_count("imports", "scripts/main.sh", "scripts/lib.sh"),
            1
        );
        assert_eq!(edge_count("calls", "main", "helper"), 1);
        assert_eq!(edge_count("calls", "jobs.build.<<", "linux"), 1);

        let yaml_keys: i64 = db
            .query_row(
                "select count(*) from symbols where repo_id=?1 and name='jobs.build.steps.uses'",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(yaml_keys, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hcl_module_imports_and_address_references_resolve() {
        let (db, root) = fixture(&[
            (
                "main.tf",
                r#"
module "vpc" {
  source = "./modules/vpc"
}

resource "aws_instance" "web" {
  ami = var.names
}
"#,
            ),
            ("variables.tf", "variable \"names\" {}\n"),
            (
                "modules/vpc/main.tf",
                "resource \"aws_s3_bucket\" \"data\" {}\n",
            ),
            ("modules/vpc/variables.tf", "variable \"cidr\" {}\n"),
            ("modules/vpc/extra.tofu", "variable \"extra\" {}\n"),
            ("modules/vpc/helper.py", "def helper(): pass\n"),
            ("modules/vpc/config.yaml", "name: unrelated\n"),
            ("modules/vpc/setup.sh", "echo setup\n"),
            ("modules/vpc/main.go", "package main\n"),
            ("modules/vpc/prod.tfvars", "cidr = \"10.0.0.0/16\"\n"),
            (
                "modules/vpc/terragrunt.hcl",
                "locals { region = \"west\" }\n",
            ),
            (
                "outputs.tf",
                "output \"id\" {\n  value = aws_instance.web.id\n}\n",
            ),
            (
                "remote.tf",
                "module \"registry\" {\n  source = \"hashicorp/consul/aws\"\n}\n",
            ),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();

        let edge_count = |kind: &str, source: &str, target: &str| -> i64 {
            db.query_row(
                "select count(*) from edges e
                   join symbols src on src.id=e.src_symbol_id
                   join symbols dst on dst.id=e.dst_symbol_id
                  where e.repo_id=?1 and e.kind=?2
                    and src.name=?3 and dst.name=?4",
                rusqlite::params![id, kind, source, target],
                |row| row.get(0),
            )
            .unwrap()
        };

        assert_eq!(edge_count("imports", "main.tf", "modules/vpc/main.tf"), 1);
        assert_eq!(
            edge_count("imports", "main.tf", "modules/vpc/variables.tf"),
            1
        );
        let imported: Vec<String> = db
            .prepare(
                "select dst.name from edges e
                 join symbols src on src.id=e.src_symbol_id
                 join symbols dst on dst.id=e.dst_symbol_id
                 where e.kind='imports' and src.name='main.tf' order by dst.name",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            imported,
            [
                "modules/vpc/extra.tofu",
                "modules/vpc/main.tf",
                "modules/vpc/variables.tf"
            ]
        );
        let registry: i64 = db
            .query_row(
                "select count(*) from edges e
                   join symbols src on src.id=e.src_symbol_id
                   join symbols dst on dst.id=e.dst_symbol_id
                  where e.repo_id=?1 and e.kind='imports'
                    and src.name='remote.tf' and dst.kind='module'",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(registry, 1, "registry sources stay external module rows");
        assert_eq!(edge_count("calls", "aws_instance.web.ami", "var.names"), 1);
        assert_eq!(
            edge_count("calls", "output.id.value", "aws_instance.web"),
            1
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn hcl_references_stay_in_the_callers_module_directory() {
        let (db, root) = fixture(&[
            (
                "variables.tf",
                "variable \"region\" {}\nresource \"terraform_data\" \"web\" {}\n",
            ),
            (
                "main.tf",
                "output \"region\" { value = var.region }\noutput \"input\" { value = terraform_data.web.input }\n",
            ),
            (
                "modules/child/main.tf",
                "variable \"region\" {}\nresource \"terraform_data\" \"web\" { input = var.region }\noutput \"input\" { value = terraform_data.web.input }\n",
            ),
            ("unrelated.yaml", "var:\n  region: unrelated\n"),
        ]);
        let edges: Vec<(String, String, String, String)> = db
            .prepare(
                "select sf.path, src.name, df.path, dst.name from edges e
                 join symbols src on src.id=e.src_symbol_id
                 join symbols dst on dst.id=e.dst_symbol_id
                 join files sf on sf.id=src.file_id join files df on df.id=dst.file_id
                 where e.kind='calls' order by sf.path, src.name",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            edges,
            vec![
                (
                    "main.tf".into(),
                    "output.input.value".into(),
                    "variables.tf".into(),
                    "terraform_data.web".into()
                ),
                (
                    "main.tf".into(),
                    "output.region.value".into(),
                    "variables.tf".into(),
                    "var.region".into()
                ),
                (
                    "modules/child/main.tf".into(),
                    "output.input.value".into(),
                    "modules/child/main.tf".into(),
                    "terraform_data.web.input".into()
                ),
                (
                    "modules/child/main.tf".into(),
                    "terraform_data.web.input".into(),
                    "modules/child/main.tf".into(),
                    "var.region".into()
                ),
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hcl_module_imports_exclude_other_languages_in_generic_hcl() {
        let (db, root) = fixture(&[
            ("main.hcl", "module child { source = \"./child\" }\n"),
            ("child/config.hcl", "settings = true\n"),
            ("child/helper.py", "def helper(): pass\n"),
        ]);
        let imports: Vec<String> = db
            .prepare("select dst.name from edges e join symbols dst on dst.id=e.dst_symbol_id where e.kind='imports'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(imports, ["child/config.hcl"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hcl_top_level_bodies_are_searchable_beyond_the_file_summary() {
        let text = format!(
            "{}description = <<EOF\nuniquebodymarker uniquebodymarker\nEOF\n",
            "# padding\n".repeat(4000)
        );
        let (db, root) = fixture(&[("prod.tfvars", &text)]);
        let repo_id = repo_id_of(&db, &root).unwrap().unwrap();
        let result = crate::ask::ask(
            &db,
            repo_id,
            &root,
            "uniquebodymarker",
            crate::ask::AskOptions {
                limit: 10,
                scope: None,
                source: false,
                full: false,
            },
        )
        .unwrap();
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].name, "description");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn hcl_provider_references_resolve_aliases_without_falling_back_to_default() {
        let (db, root) = fixture(&[
            (
                "providers.tf",
                "provider \"aws\" {}\nprovider \"aws\" { alias = \"west\" }\nprovider \"aws\" { alias = \"east\" }\n",
            ),
            (
                "main.tf",
                r#"
resource "aws_instance" "west" { provider = aws.west }
resource "aws_instance" "default" { provider = aws }
resource "aws_instance" "missing" { provider = aws.missing }
module "child" {
  source = "./child"
  providers = { aws.east = aws.west }
}
"#,
            ),
            (
                "child/main.tf",
                "provider \"aws\" { alias = \"west\" }\nresource \"aws_instance\" \"west\" { provider = aws.west }\n",
            ),
        ]);
        let edges: Vec<(String, String, String, String)> = db
            .prepare(
                "select sf.path, src.name, df.path, dst.name from edges e
                 join symbols src on src.id=e.src_symbol_id
                 join symbols dst on dst.id=e.dst_symbol_id
                 join files sf on sf.id=src.file_id join files df on df.id=dst.file_id
                 where e.kind='calls' order by sf.path, src.name",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            edges,
            vec![
                (
                    "child/main.tf".into(),
                    "aws_instance.west.provider".into(),
                    "child/main.tf".into(),
                    "provider.aws.west".into()
                ),
                (
                    "main.tf".into(),
                    "aws_instance.default.provider".into(),
                    "providers.tf".into(),
                    "provider.aws".into()
                ),
                (
                    "main.tf".into(),
                    "aws_instance.west.provider".into(),
                    "providers.tf".into(),
                    "provider.aws.west".into()
                ),
                (
                    "main.tf".into(),
                    "module.child.providers.aws.east".into(),
                    "providers.tf".into(),
                    "provider.aws.west".into()
                ),
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn callers_aggregates_symbols_defined_by_a_file_seed() {
        let (db, root) = fixture(&[
            ("src/a.ts", "export function target() {}\n"),
            ("src/b.ts", "function run() { target(); }\n"),
        ]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let (_, reached) = callers(&db, id, "src/a.ts", false, 2).unwrap();
        assert!(
            reached
                .iter()
                .any(|hit| hit.name == "run" && hit.edge == "calls")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn repo_map_counts_files_without_definitions() {
        let (db, root) = fixture(&[("config/settings.py", "VALUE = 1\n")]);
        let id = repo_id_of(&db, &root).unwrap().unwrap();
        let map = repo_map(&db, id, 12).unwrap();
        let config = map.dirs.iter().find(|d| d.dir == "config/").unwrap();
        assert_eq!(config.files, 1);
        assert_eq!(config.symbols, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rebuilding_replaces_rather_than_duplicates() {
        let (mut db, root) = fixture(&[("src/a.ts", "export function only() {}\n")]);
        let before: i64 = db
            .query_row("select count(*) from symbols", [], |r| r.get(0))
            .unwrap();
        let cached = build(&mut db, &root).unwrap();
        let after: i64 = db
            .query_row("select count(*) from symbols", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            before, after,
            "a second build must not double the symbol rows"
        );
        assert_eq!(
            cached.parsed, 0,
            "unchanged files must replay extraction cache"
        );
        assert_eq!(cached.reused, 1);

        std::fs::write(root.join("src/a.ts"), "export function changed() {}\n").unwrap();
        let changed = build(&mut db, &root).unwrap();
        assert_eq!(changed.parsed, 1);
        assert_eq!(changed.reused, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn incremental_build_reuses_unchanged_files_and_removes_deleted_files() {
        let (mut db, root) = fixture(&[
            ("src/a.ts", "export function a() {}\n"),
            ("src/b.ts", "export function b() {}\n"),
        ]);
        std::fs::write(root.join("src/a.ts"), "export function a2() {}\n").unwrap();
        std::fs::remove_file(root.join("src/b.ts")).unwrap();

        let stats = build(&mut db, &root).unwrap();
        assert_eq!(stats.parsed, 1);
        assert_eq!(stats.reused, 0);
        assert_eq!(stats.deleted, 1);
        let files: i64 = db
            .query_row("select count(*) from files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(files, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parallel_extraction_preserves_serial_graph_order() {
        use std::sync::atomic::{AtomicU32, Ordering};

        static N: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "panoptes-parallel-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        for index in 0usize..130 {
            let previous = index.saturating_sub(1);
            let body = if index == 0 {
                "export function item0() { return 0; }\n".to_string()
            } else {
                format!(
                    "import {{ item{previous} }} from './f{previous}';\nexport function item{index}() {{ return item{previous}(); }}\n"
                )
            };
            std::fs::write(root.join(format!("src/f{index}.ts")), body).unwrap();
        }

        let mut serial = crate::db::open(&root.join("serial.db")).unwrap();
        let mut parallel = crate::db::open(&root.join("parallel.db")).unwrap();
        let serial_stats = build_with_jobs(&mut serial, &root, 1).unwrap();
        let parallel_stats = build_with_jobs(&mut parallel, &root, 4).unwrap();
        assert_eq!(serial_stats.files, parallel_stats.files);
        assert_eq!(serial_stats.symbols, parallel_stats.symbols);
        assert_eq!(serial_stats.edges, parallel_stats.edges);

        fn symbols(db: &Connection) -> Vec<(String, String, String, i64, i64, Option<String>)> {
            let mut statement = db
                .prepare(
                    "select f.path, s.name, s.kind, s.start_line, s.end_line, s.container
                       from symbols s join files f on f.id=s.file_id
                      order by f.path, s.id",
                )
                .unwrap();
            statement
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                })
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        }

        fn edges(db: &Connection) -> Vec<(String, String, String, String, String)> {
            let mut statement = db
                .prepare(
                    "select sf.path, src.name, df.path, dst.name, e.kind
                       from edges e
                       join symbols src on src.id=e.src_symbol_id
                       join files sf on sf.id=src.file_id
                       join symbols dst on dst.id=e.dst_symbol_id
                       join files df on df.id=dst.file_id
                      order by sf.path, src.id, df.path, dst.id, e.kind",
                )
                .unwrap();
            statement
                .query_map([], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap()
        }

        assert_eq!(symbols(&serial), symbols(&parallel));
        assert_eq!(edges(&serial), edges(&parallel));
        drop((serial, parallel));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn freshness_reports_added_modified_and_deleted_paths() {
        let (db, root) = fixture(&[
            ("src/a.ts", "export function a() {}\n"),
            ("src/b.ts", "export function b() {}\n"),
        ]);
        assert!(freshness(&db, &root).unwrap().is_clean());
        std::fs::write(root.join("src/a.ts"), "export function a2() {}\n").unwrap();
        std::fs::remove_file(root.join("src/b.ts")).unwrap();
        std::fs::write(root.join("src/c.ts"), "export function c() {}\n").unwrap();

        let state = freshness(&db, &root).unwrap();
        assert_eq!(state.modified, ["src/a.ts"]);
        assert_eq!(state.deleted, ["src/b.ts"]);
        assert_eq!(state.added, ["src/c.ts"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
