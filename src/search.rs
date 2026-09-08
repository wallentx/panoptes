//! Persistent, lossless search terms shared by indexing and retrieval.

use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

pub const DDL: &str = r#"
create table if not exists search_terms (
  repo_id integer not null references repos(id) on delete cascade,
  term text not null,
  symbol_id integer not null references symbols(id) on delete cascade,
  name_count integer not null,
  path_count integer not null,
  signature_count integer not null,
  body_count integer not null,
  primary key (repo_id, term, symbol_id)
) without rowid;
create index if not exists search_terms_by_symbol on search_terms(symbol_id);
"#;

/// Index each field separately so candidate filtering preserves lexical ranking.
pub fn index_symbol(db: &Connection, symbol_id: i64) -> Result<()> {
    let (repo_id, name, path, signature, body): (i64, String, String, String, String) = db
        .query_row(
            "select s.repo_id, s.name, f.path, coalesce(s.signature,''), coalesce(s.summary,'')
         from symbols s join files f on f.id=s.file_id where s.id=?1",
            [symbol_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
    let mut counts: BTreeMap<String, [i64; 4]> = BTreeMap::new();
    for (field, text) in [&name, &path, &signature, &body].into_iter().enumerate() {
        for term in terms(text) {
            counts.entry(term).or_default()[field] += 1;
        }
    }
    let mut insert = db.prepare_cached(
        "insert into search_terms(repo_id,term,symbol_id,name_count,path_count,signature_count,body_count)
         values (?1,?2,?3,?4,?5,?6,?7)",
    )?;
    for (term, [name, path, signature, body]) in counts {
        insert.execute(params![
            repo_id, term, symbol_id, name, path, signature, body
        ])?;
    }
    Ok(())
}

/// Populate the new cache from an existing snapshot, without rereading source.
pub fn backfill(db: &Connection) -> Result<()> {
    let mut statement = db.prepare("select id from symbols where kind != 'module' order by id")?;
    let ids = statement.query_map([], |r| r.get::<_, i64>(0))?;
    for id in ids {
        index_symbol(db, id?)?;
    }
    Ok(())
}

fn stopword(term: &str) -> bool {
    matches!(
        term,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "code"
            | "does"
            | "for"
            | "from"
            | "how"
            | "in"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "that"
            | "the"
            | "this"
            | "to"
            | "what"
            | "where"
            | "which"
            | "with"
    )
}

pub fn terms(text: &str) -> Vec<String> {
    if text.is_ascii() {
        return ascii_terms(text);
    }
    unicode_terms(text)
}

fn ascii_terms(text: &str) -> Vec<String> {
    // The standard library bulk lowercase operation can vectorize without
    // requiring a stronger CPU baseline or custom unsafe loads and tails.
    let mut lowered = text.to_owned();
    lowered.make_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &byte) in bytes.iter().enumerate() {
        let separator = !byte.is_ascii_alphanumeric();
        let camel = byte.is_ascii_uppercase() && i > 0 && bytes[i - 1].is_ascii_lowercase();
        if separator || camel {
            push_term(&mut out, &lowered[start..i]);
            start = if separator { i + 1 } else { i };
        }
    }
    push_term(&mut out, &lowered[start..]);
    out
}

fn push_term(out: &mut Vec<String>, term: &str) {
    if term.len() > 1 && !stopword(term) {
        out.push(term.to_owned());
    }
}

fn unicode_terms(text: &str) -> Vec<String> {
    let mut expanded = String::with_capacity(text.len() * 2);
    let mut previous_lower = false;
    for ch in text.chars() {
        if ch.is_uppercase() && previous_lower {
            expanded.push(' ');
        }
        if ch.is_alphanumeric() || ch == '_' {
            expanded.extend(ch.to_lowercase());
            previous_lower = ch.is_lowercase();
        } else {
            expanded.push(' ');
            previous_lower = false;
        }
    }
    expanded
        .split(|ch: char| ch == '_' || ch.is_whitespace())
        .filter(|term| term.len() > 1 && !stopword(term))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_terms_follow_edits_renames_and_deletions() {
        use crate::{ask, db, index};
        let root =
            std::env::temp_dir().join(format!("panoptes-search-cache-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let path = root.join("src/first.ts");
        std::fs::write(&path, "export function oldMarker() { return 'oldMarker'; }").unwrap();
        let mut conn = db::open(&root.join("test.db")).unwrap();
        index::build(&mut conn, &root).unwrap();
        let repo_id = index::repo_id_of(&conn, &root).unwrap().unwrap();
        let names = |query: &str, conn: &Connection| {
            ask::ask(
                conn,
                repo_id,
                &root,
                query,
                ask::AskOptions {
                    limit: 20,
                    scope: Some("src"),
                    source: false,
                    full: false,
                },
            )
            .unwrap()
            .hits
            .into_iter()
            .map(|h| (h.name, h.path))
            .collect::<Vec<_>>()
        };
        assert!(
            names("oldMarker", &conn)
                .iter()
                .any(|(name, _)| name == "oldMarker")
        );
        std::fs::write(&path, "export function newMarker() { return 'newMarker'; }").unwrap();
        let renamed = root.join("src/renamed.ts");
        std::fs::rename(&path, &renamed).unwrap();
        index::build(&mut conn, &root).unwrap();
        assert!(names("old", &conn).is_empty());
        assert!(
            names("newMarker", &conn)
                .iter()
                .any(|(name, path)| name == "newMarker" && path == "src/renamed.ts")
        );
        let count: i64 = conn
            .query_row("select count(*) from search_terms", [], |r| r.get(0))
            .unwrap();
        index::build(&mut conn, &root).unwrap();
        assert_eq!(
            count,
            conn.query_row("select count(*) from search_terms", [], |r| r
                .get::<_, i64>(0))
                .unwrap()
        );
        std::fs::remove_file(renamed).unwrap();
        index::build(&mut conn, &root).unwrap();
        assert!(names("newMarker", &conn).is_empty());
        assert_eq!(
            0,
            conn.query_row("select count(*) from search_terms", [], |r| r
                .get::<_, i64>(0))
                .unwrap()
        );
        drop(conn);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ascii_fast_path_matches_unicode_reference_at_boundaries_and_offsets() {
        for length in [0, 1, 7, 15, 16, 17, 31, 32, 33, 63, 64, 65, 255, 4096] {
            let source =
                "aZ_fooBar99 HTTPServer /path.to/SomeFile.ts; the and API".repeat(length / 5 + 2);
            for offset in 0..32 {
                let text = &source[offset..offset + length];
                assert_eq!(
                    ascii_terms(text),
                    unicode_terms(text),
                    "length={length}, offset={offset}"
                );
            }
        }
        let mut random = 17u64;
        for _ in 0..512 {
            let mut text = String::new();
            for _ in 0..257 {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                text.push(char::from((random & 127) as u8));
            }
            assert_eq!(ascii_terms(&text), unicode_terms(&text));
        }
    }

    #[test]
    fn unicode_case_expansion_and_boundaries_are_preserved() {
        for text in [
            "caféValue",
            "İstanbul Straße",
            "東京_変数",
            "ßABC",
            "xΣigma",
            "a\u{2003}bç",
            "é_ø_中",
            "🙂fooBar",
            "e\u{301}clair",
            "aǅValue",
        ] {
            assert_eq!(terms(text), unicode_terms(text));
        }
        assert_eq!(terms("caféValue"), ["café", "value"]);
        assert_eq!(terms("İstanbul"), ["i\u{307}stanbul"]);
        assert_eq!(terms("getUserPermissions"), ["get", "user", "permissions"]);
    }
}
