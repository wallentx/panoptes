//! Ranked lexical retrieval over the SQLite symbol corpus.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
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
    root: &Path,
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
                                root,
                                &reached.path,
                                reached.start_line,
                                reached.end_line,
                                options.full,
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
    let mut statement = db.prepare(
        "select s.id,s.name,s.kind,f.path,s.start_line,s.end_line,coalesce(s.signature,''),
                (select count(*) from edges e where e.repo_id=s.repo_id and e.dst_symbol_id=s.id),
                t.name_count,t.path_count,t.signature_count,t.body_count
         from search_terms t join symbols s on s.id=t.symbol_id join files f on f.id=s.file_id
         where t.repo_id=?1 and t.term=?2 and s.kind != 'module'
         and (?3 is null or f.path=?3 or substr(f.path,1,length(?3)+1)=?3||'/')",
    )?;
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
                    in_edges: row.get(7)?,
                    term_counts: Vec::with_capacity(unique_terms.len().min(4)),
                }),
            };
            let mut counts = [0.0; 4];
            for (field, count) in counts.iter_mut().enumerate() {
                *count = row.get::<_, i64>(8 + field)? as f64;
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
        score *= 1.0 + (document.in_edges as f64 + 1.0).ln() * 0.06;
        scored.push((score, document));
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
                root,
                &document.path,
                document.start_line,
                document.end_line,
                options.full,
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

fn source_excerpt(root: &Path, path: &str, start: i64, end: i64, full: bool) -> Option<String> {
    let text = std::fs::read_to_string(root.join(path)).ok()?;
    let start = start.max(1) as usize;
    let mut end = end.max(start as i64) as usize;
    if !full {
        end = end.min(start + 7);
    }
    Some(
        text.lines()
            .enumerate()
            .filter(|(index, _)| start <= index + 1 && *index < end)
            .map(|(_, line)| line)
            .collect::<Vec<_>>()
            .join("\n"),
    )
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
