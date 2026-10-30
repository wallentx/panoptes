//! Ranked lexical retrieval over the SQLite symbol corpus.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

use crate::search::terms;

#[derive(Debug, Serialize)]
pub struct AskHit {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub path: String,
    pub start_line: i64,
    pub end_line: i64,
    pub signature: String,
    pub score: f64,
    pub in_edges: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AskResult {
    pub query: String,
    pub mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub hits: Vec<AskHit>,
}

struct Document {
    id: i64,
    name: String,
    kind: String,
    path: String,
    start_line: i64,
    end_line: i64,
    signature: String,
    in_edges: i64,
    term_counts: Vec<(usize, [f64; 4])>,
}

pub struct AskOptions<'a> {
    pub limit: usize,
    pub scope: Option<&'a str>,
    pub source: bool,
    pub full: bool,
}

pub fn ask(
    db: &Connection,
    repo_id: i64,
    _root: &Path,
    query: &str,
    options: AskOptions<'_>,
) -> Result<AskResult> {
    // Candidate postings, corpus size and edge counts must describe one snapshot
    // even when another MCP process commits an index update during the query.
    let _snapshot = if db.is_autocommit() {
        Some(db.unchecked_transaction()?)
    } else {
        None
    };
    let mut source_cache = SourceCache::default();
    if let Some((subject, outgoing)) = structural_subject(query) {
        let (seeds, reached) =
            crate::index::callers_scoped(db, repo_id, subject, outgoing, 1, options.scope)?;
        if !seeds.is_empty() && !reached.is_empty() {
            let hits = reached
                .into_iter()
                .take(options.limit.max(1))
                .map(|reached| AskHit {
                    id: reached.id,
                    name: reached.name,
                    kind: reached.kind,
                    path: reached.path.clone(),
                    start_line: reached.start_line,
                    end_line: reached.end_line,
                    signature: reached.signature,
                    score: 1.0 / reached.depth as f64,
                    in_edges: reached.in_edges,
                    source: options
                        .source
                        .then(|| {
                            source_excerpt(
                                db,
                                repo_id,
                                &reached.path,
                                reached.start_line,
                                reached.end_line,
                                options.full,
                                &mut source_cache,
                            )
                        })
                        .flatten(),
                })
                .collect();
            return Ok(AskResult {
                query: query.to_string(),
                mode: if outgoing {
                    "structural-callees"
                } else {
                    "structural-callers"
                },
                scope: options.scope.map(str::to_string),
                note: None,
                hits,
            });
        }
    }
    let query_terms = terms(query);
    if query_terms.is_empty() {
        return Ok(AskResult {
            query: query.to_string(),
            mode: "empty",
            scope: options.scope.map(str::to_string),
            note: None,
            hits: Vec::new(),
        });
    }

    let scope = options.scope.map(|scope| scope.trim_matches('/'));
    let corpus_size: i64 = db.query_row(
        "select count(*) from symbols s join files f on f.id=s.file_id
         where s.repo_id=?1 and s.kind != 'module'
         and (?2 is null or f.path=?2 or substr(f.path,1,length(?2)+1)=?2||'/')",
        rusqlite::params![repo_id, scope],
        |r| r.get(0),
    )?;
    // With only one distinct term, each symbol has one posting, so the
    // correlated indexed count already runs exactly once and avoids a second
    // query. Multi-term searches defer counts until after candidate filtering.
    let count_in_posting = query_terms.iter().all(|term| term == &query_terms[0]);
    let edge_projection = if count_in_posting {
        "(select count(*) from edges e where e.repo_id=s.repo_id and e.dst_symbol_id=s.id),"
    } else {
        ""
    };
    let mut statement = db.prepare(&format!(
        "select s.id,s.name,s.kind,f.path,s.start_line,s.end_line,coalesce(s.signature,''),
                {edge_projection}t.name_count,t.path_count,t.signature_count,t.body_count
         from search_terms t join symbols s on s.id=t.symbol_id join files f on f.id=s.file_id
         where t.repo_id=?1 and t.term=?2 and s.kind != 'module'
         and (?3 is null or f.path=?3 or substr(f.path,1,length(?3)+1)=?3||'/')"
    ))?;
    // Deduplicate lookup work while retaining repeated query terms in scoring.
    let mut term_slots: HashMap<&str, usize> = HashMap::new();
    let mut unique_terms = Vec::new();
    let mut query_slots = Vec::new();
    for term in &query_terms {
        let next = unique_terms.len();
        let slot = *term_slots.entry(term).or_insert(next);
        if slot == next {
            unique_terms.push(term);
        }
        query_slots.push(slot);
    }
    let mut document_frequency = vec![0usize; unique_terms.len()];
    let mut candidates: HashMap<i64, Document> = HashMap::new();
    for (slot, term) in unique_terms.iter().enumerate() {
        let mut rows = statement.query(rusqlite::params![repo_id, term, scope])?;
        while let Some(row) = rows.next()? {
            let id = row.get(0)?;
            let document = match candidates.entry(id) {
                std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::hash_map::Entry::Vacant(entry) => entry.insert(Document {
                    id,
                    name: row.get(1)?,
                    kind: row.get(2)?,
                    path: row.get(3)?,
                    start_line: row.get(4)?,
                    end_line: row.get(5)?,
                    signature: row.get(6)?,
                    in_edges: if count_in_posting { row.get(7)? } else { 0 },
                    term_counts: Vec::with_capacity(unique_terms.len().min(4)),
                }),
            };
            let mut counts = [0.0; 4];
            for (field, count) in counts.iter_mut().enumerate() {
                *count = row.get::<_, i64>(7 + usize::from(count_in_posting) + field)? as f64;
            }
            // Only matched terms occupy memory; long queries must not allocate
            // a full query-by-corpus matrix. Slots arrive in increasing order.
            document.term_counts.push((slot, counts));
            document_frequency[slot] += 1;
        }
    }
    let corpus_size = corpus_size.max(1) as f64;
    let query_mentions_tests = query_terms
        .iter()
        .any(|term| matches!(term.as_str(), "test" | "tests" | "fixture" | "fixtures"));

    let idfs: Vec<f64> = document_frequency
        .iter()
        .map(|&df| ((corpus_size + 1.0) / (df as f64 + 1.0)).ln() + 1.0)
        .collect();
    let mut scored = Vec::new();
    for document in candidates.into_values() {
        let mut score = 0.0;
        let mut strong = 0.0;
        let mut possible = 0.0;
        for &slot in &query_slots {
            let idf = idfs[slot];
            possible += idf;
            let [name, path, signature, body] = document
                .term_counts
                .binary_search_by_key(&slot, |(index, _)| *index)
                .map(|index| document.term_counts[index].1)
                .unwrap_or([0.0; 4]);
            if name + path + signature > 0.0 {
                strong += idf;
            }
            score += idf * (name * 5.0 + path * 2.5 + signature * 2.0 + body.min(3.0));
        }
        if score == 0.0 {
            continue;
        }
        let strong_share = if possible == 0.0 {
            0.0
        } else {
            strong / possible
        };
        if strong_share == 0.0 && score < possible * 1.5 {
            continue;
        }
        if is_test_path(&document.path) && !query_mentions_tests {
            score *= 0.65;
        }
        scored.push((score, document));
    }
    // Incoming edges affect ranking only after the lexical eligibility filter.
    // For multi-term searches, count once per surviving symbol rather than
    // once per query-term posting.
    if !count_in_posting {
        let ids: Vec<_> = scored.iter().map(|(_, document)| document.id).collect();
        let edge_counts = incoming_edge_counts(db, repo_id, &ids)?;
        for (_, document) in &mut scored {
            document.in_edges = edge_counts.get(&document.id).copied().unwrap_or(0);
        }
    }
    for (score, document) in &mut scored {
        *score *= 1.0 + (document.in_edges as f64 + 1.0).ln() * 0.06;
    }

    scored.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.start_line.cmp(&right.start_line))
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut hits = Vec::new();
    for (score, document) in scored.into_iter().take(options.limit.max(1)) {
        let source = if options.source {
            source_excerpt(
                db,
                repo_id,
                &document.path,
                document.start_line,
                document.end_line,
                options.full,
                &mut source_cache,
            )
        } else {
            None
        };
        hits.push(AskHit {
            id: document.id,
            name: document.name,
            kind: document.kind,
            path: document.path,
            start_line: document.start_line,
            end_line: document.end_line,
            signature: document.signature,
            score,
            in_edges: document.in_edges,
            source,
        });
    }

    Ok(AskResult {
        query: query.to_string(),
        mode: "lexical",
        scope: options.scope.map(str::to_string),
        note: structural_subject(query).map(|(subject, _)| {
            format!("no resolved graph edges for {subject:?}; showing lexical matches")
        }),
        hits,
    })
}

fn incoming_edge_counts(db: &Connection, repo_id: i64, ids: &[i64]) -> Result<HashMap<i64, i64>> {
    // Stay comfortably below SQLite's historical 999-variable limit while
    // amortizing statement/row setup over many candidates.
    const BATCH_SIZE: usize = 256;
    let mut counts = HashMap::new();
    for batch in ids.chunks(BATCH_SIZE) {
        let placeholders = (0..batch.len())
            .map(|index| format!("?{}", index + 2))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "select dst_symbol_id,count(*) from edges
             where repo_id=?1 and dst_symbol_id in ({placeholders})
             group by dst_symbol_id"
        );
        let mut statement = db.prepare_cached(&sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(
            std::iter::once(repo_id).chain(batch.iter().copied()),
        ))?;
        while let Some(row) = rows.next()? {
            let id: i64 = row.get(0)?;
            counts.insert(id, row.get(1)?);
        }
    }
    Ok(counts)
}

fn structural_subject(query: &str) -> Option<(&str, bool)> {
    let query = query.trim().trim_end_matches(['?', '.']);
    let lower = query.to_ascii_lowercase();
    for prefix in ["who calls ", "what calls "] {
        if lower.starts_with(prefix) {
            return Some((query[prefix.len()..].trim(), false));
        }
    }
    for prefix in ["what does ", "what do "] {
        if lower.starts_with(prefix) && lower.ends_with(" call") {
            return Some((query[prefix.len()..query.len() - 5].trim(), true));
        }
    }
    None
}

#[cfg(test)]
fn in_scope(path: &str, scope: &str) -> bool {
    let scope = scope.trim_matches('/');
    path == scope || path.starts_with(&format!("{scope}/"))
}

fn is_test_path(path: &str) -> bool {
    let file = path.rsplit('/').next().unwrap_or(path);
    path.split('/')
        .any(|part| matches!(part, "test" | "tests" | "__tests__"))
        || file.starts_with("test_")
        || file == "conftest.py"
        || file.contains(".test.")
        || file.contains("_test.")
}

// Cache captured source only for this read view. The fixed byte budget avoids
// retaining several large BLOBs when ranked results span different files.
#[derive(Default)]
struct SourceCache {
    files: HashMap<String, Option<String>>,
    bytes: usize,
}

impl SourceCache {
    const MAX_BYTES: usize = 1024 * 1024;

    fn read<'a>(&'a mut self, db: &Connection, snapshot: i64, path: &str) -> Option<Cow<'a, str>> {
        match self.files.entry(path.to_owned()) {
            std::collections::hash_map::Entry::Occupied(entry) => {
                entry.into_mut().as_deref().map(Cow::Borrowed)
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                let source = crate::content::source(db, snapshot, path).ok().flatten();
                let bytes = source.as_ref().map_or(0, String::len);
                if bytes <= Self::MAX_BYTES.saturating_sub(self.bytes) {
                    self.bytes += bytes;
                    entry.insert(source).as_deref().map(Cow::Borrowed)
                } else {
                    source.map(Cow::Owned)
                }
            }
        }
    }
}

pub(crate) const MAX_EXCERPT_BYTES: usize = 1024;
const EXCERPT_TRUNCATED: &str = "\n[excerpt truncated; use full=true]";

fn source_excerpt(
    db: &Connection,
    snapshot: i64,
    path: &str,
    start: i64,
    end: i64,
    full: bool,
    cache: &mut SourceCache,
) -> Option<String> {
    let text = cache.read(db, snapshot, path)?;
    let start = start.max(1) as usize;
    let mut end = end.max(start as i64) as usize;
    if !full {
        end = end.min(start + 7);
    }
    let mut excerpt = text
        .lines()
        .skip(start - 1)
        .take(end - start + 1)
        .collect::<Vec<_>>()
        .join("\n");
    if !full && excerpt.len() > MAX_EXCERPT_BYTES {
        let mut boundary = MAX_EXCERPT_BYTES - EXCERPT_TRUNCATED.len();
        while !excerpt.is_char_boundary(boundary) {
            boundary -= 1;
        }
        excerpt.truncate(boundary);
        excerpt.push_str(EXCERPT_TRUNCATED);
    }
    Some(excerpt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenization_splits_camel_case_and_drops_junk() {
        assert_eq!(
            terms("How does serverEntry work?"),
            ["server", "entry", "work"]
        );
    }

    #[test]
    fn scope_is_segment_aware() {
        assert!(in_scope("src/graph/build.rs", "src/graph"));
        assert!(!in_scope("src/graphical/a.rs", "src/graph"));
    }

    #[test]
    fn structural_prompts_extract_the_subject_and_direction() {
        assert_eq!(
            structural_subject("Who calls Cache.get?"),
            Some(("Cache.get", false))
        );
        assert_eq!(
            structural_subject("what does Cache.get call?"),
            Some(("Cache.get", true))
        );
        assert_eq!(structural_subject("cache lookup"), None);
    }

    #[test]
    fn incoming_edge_batches_keep_counts_scoped_and_include_zero_edge_candidates() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch("create table edges(repo_id integer,src_symbol_id integer,dst_symbol_id integer); create index edges_by_dst on edges(repo_id,dst_symbol_id);").unwrap();
        let ids: Vec<i64> = (1..=600).collect();
        for &id in &ids {
            for source in 0..id % 4 {
                db.execute(
                    "insert into edges values (1,?1,?2)",
                    rusqlite::params![source, id],
                )
                .unwrap();
            }
            // A different snapshot must not inflate this candidate's count.
            db.execute("insert into edges values (2,0,?1)", [id])
                .unwrap();
        }
        assert!(incoming_edge_counts(&db, 1, &[]).unwrap().is_empty());
        let counts = incoming_edge_counts(&db, 1, &ids).unwrap();
        for id in ids {
            assert_eq!(
                counts.get(&id).copied().unwrap_or(0),
                id % 4,
                "candidate {id}"
            );
        }
        let plan: String = db.query_row("explain query plan select dst_symbol_id,count(*) from edges where repo_id=1 and dst_symbol_id in (1,2,3) group by dst_symbol_id", [], |row| row.get(3)).unwrap();
        assert!(
            plan.contains("SEARCH edges USING COVERING INDEX edges_by_dst"),
            "{plan}"
        );
    }

    #[test]
    fn ranking_is_stable_de_ranks_tests_and_searches_module_bodies() {
        use crate::{db, index};
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "panoptes-ask-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(
            root.join("src/gateway.ts"),
            "const moduleMarker = 'only_at_module_scope';\nexport function processPayment() { return moduleMarker; }\nexport function main() { return processPayment(); }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tests/gateway.test.ts"),
            "export function processPaymentTest() { return 'payment'; }\n",
        )
        .unwrap();
        let store = root.join("store.db");
        let mut conn = db::open(&store).unwrap();
        index::build(&mut conn, &root).unwrap();
        let repo_id = index::repo_id_of(&conn, &root).unwrap().unwrap();
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = reads.clone();
        conn.authorizer(Some(move |context: rusqlite::hooks::AuthContext<'_>| {
            if matches!(
                context.action,
                rusqlite::hooks::AuthAction::Read {
                    table_name: "content_objects",
                    column_name: "source_bytes"
                }
            ) {
                observed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            rusqlite::hooks::Authorization::Allow
        }))
        .unwrap();
        let mut cache = SourceCache::default();
        assert!(
            source_excerpt(&conn, repo_id, "src/gateway.ts", 2, 2, false, &mut cache)
                .unwrap()
                .contains("processPayment")
        );
        let first_reads = reads.load(std::sync::atomic::Ordering::Relaxed);
        assert!(first_reads > 0);
        assert!(
            source_excerpt(&conn, repo_id, "src/gateway.ts", 3, 3, true, &mut cache)
                .unwrap()
                .contains("main")
        );
        assert_eq!(
            reads.load(std::sync::atomic::Ordering::Relaxed),
            first_reads
        );
        source_excerpt(
            &conn,
            repo_id,
            "src/gateway.ts",
            2,
            2,
            false,
            &mut SourceCache::default(),
        )
        .unwrap();
        assert!(reads.load(std::sync::atomic::Ordering::Relaxed) > first_reads);
        conn.authorizer(
            None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
        )
        .unwrap();

        let first = ask(
            &conn,
            repo_id,
            &root,
            "process payment",
            AskOptions {
                limit: 8,
                scope: None,
                source: false,
                full: false,
            },
        )
        .unwrap();
        assert_eq!(first.hits[0].path, "src/gateway.ts");
        for hit in &first.hits {
            let expected: i64 = conn
                .query_row(
                    "select count(*) from edges where repo_id=?1 and dst_symbol_id=?2",
                    rusqlite::params![repo_id, hit.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(hit.in_edges, expected);
        }
        let repeated = ask(
            &conn,
            repo_id,
            &root,
            "process payment process payment",
            AskOptions {
                limit: 8,
                scope: None,
                source: false,
                full: false,
            },
        )
        .unwrap();
        assert_eq!(first.hits.len(), repeated.hits.len());
        for (single, repeated) in first.hits.iter().zip(&repeated.hits) {
            assert_eq!(single.id, repeated.id);
            assert_eq!(single.in_edges, repeated.in_edges);
            assert!((single.score * 2.0 - repeated.score).abs() < 1e-9);
        }

        let ids: Vec<_> = first.hits.iter().map(|hit| hit.id).collect();
        index::build(&mut conn, &root).unwrap();
        let second = ask(
            &conn,
            repo_id,
            &root,
            "process payment",
            AskOptions {
                limit: 8,
                scope: None,
                source: false,
                full: false,
            },
        )
        .unwrap();
        assert_eq!(
            ids,
            second.hits.iter().map(|hit| hit.id).collect::<Vec<_>>()
        );
        let module = ask(
            &conn,
            repo_id,
            &root,
            "only_at_module_scope",
            AskOptions {
                limit: 8,
                scope: None,
                source: false,
                full: false,
            },
        )
        .unwrap();
        assert!(module.hits.iter().any(|hit| hit.path == "src/gateway.ts"));
        let structural = ask(
            &conn,
            repo_id,
            &root,
            "who calls processPayment?",
            AskOptions {
                limit: 8,
                scope: None,
                source: false,
                full: false,
            },
        )
        .unwrap();
        assert_eq!(structural.mode, "structural-callers");
        assert!(structural.hits.iter().any(|hit| hit.name == "main"));
        drop(conn);
        let _ = std::fs::remove_dir_all(root);
    }
}
