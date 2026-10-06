//! The store: one SQLite database, all repos, outside every working tree.
//!
//! Location is `$XDG_DATA_HOME/panoptes/panoptes.db`, falling back to
//! `~/.local/share/panoptes/panoptes.db`. Nothing is ever written into an indexed
//! repository — a repo is identified by its path, not by a marker file inside it.
//!
//! Schema notes that are load-bearing rather than incidental:
//!
//! * `checkouts.root` is a canonical selector for a stable checkout UID and its
//!   active snapshot. Identity metadata and graph ownership are separate.
//! * `snapshots.extractor_stamp` and a complete input manifest govern graph
//!   freshness. Legacy graphs have no verified manifest until they are rebuilt.
//! * Legacy `repo_id` column names now reference immutable snapshot owners.
//!   Source objects and base extractions are shared by strong content/profile keys.
//! * `symbols_fts` is an external-content FTS5 table over `symbols`, kept in sync
//!   by triggers. External content means the text is not stored twice; the
//!   triggers are mandatory, not an optimization, because a contentless FTS index
//!   silently returns stale rows if they drift.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Bumped whenever the DDL below changes in a way an existing store cannot serve.
/// Read from and written to `pragma user_version`.
pub const SCHEMA_VERSION: i64 = 7;

// Historical v1 schema: migrations preserve its row IDs while evolving ownership.
const DDL: &str = r#"
create table if not exists repos (
  id              integer primary key,
  root            text    not null unique,
  git_common_dir  text,
  indexed_at      integer not null,
  extractor_stamp text    not null
);

create table if not exists files (
  id       integer primary key,
  repo_id  integer not null references repos(id) on delete cascade,
  path     text    not null,
  mtime    integer not null,
  size     integer not null,
  hash     text    not null,
  unique (repo_id, path)
);

-- Parsed extraction payload for incremental builds. Symbol rows remain the
-- query surface; this cache retains unresolved calls/imports/bindings so edges
-- can be rebuilt repo-wide without reparsing unchanged source files.
create table if not exists file_extracts (
  file_id          integer primary key references files(id) on delete cascade,
  extractor_stamp  text    not null,
  payload           text    not null
);

create table if not exists symbols (
  id         integer primary key,
  repo_id    integer not null references repos(id) on delete cascade,
  file_id    integer not null references files(id) on delete cascade,
  name       text    not null,
  kind       text    not null,
  start_line integer not null,
  end_line   integer not null,
  signature  text,
  crux       text,
  summary    text,
  -- Enclosing class for a method, else null. This is what lets a member call
  -- resolve: `repo.scan()` binds `repo` to type Repo, and the edge target is the
  -- symbol named `scan` whose container is `Repo`. Without it every method named
  -- `scan` in the repo is an equally good candidate and the edge is dropped.
  container  text
);
create index if not exists symbols_by_name on symbols(repo_id, name);
create index if not exists symbols_by_file on symbols(file_id);
create index if not exists symbols_by_container on symbols(repo_id, container, name);

create table if not exists edges (
  repo_id       integer not null references repos(id) on delete cascade,
  src_symbol_id integer not null references symbols(id) on delete cascade,
  dst_symbol_id integer not null references symbols(id) on delete cascade,
  kind          text    not null
);
-- An edge is a relationship, not an occurrence: three calls to the same function
-- from one caller are one edge. Without this, in-edge counts inflate and callers
-- lists the same caller once per call site.
create unique index if not exists edges_unique
  on edges(repo_id, src_symbol_id, dst_symbol_id, kind);
-- callers(x) walks dst->src; --direction out walks src->dst. Both need an index
-- or every traversal degrades to a full scan of the repo's edge set.
create index if not exists edges_by_dst on edges(repo_id, dst_symbol_id);
create index if not exists edges_by_src on edges(repo_id, src_symbol_id);

create virtual table if not exists symbols_fts using fts5(
  name, signature, summary,
  content='symbols',
  content_rowid='id'
);

create trigger if not exists symbols_ai after insert on symbols begin
  insert into symbols_fts(rowid, name, signature, summary)
  values (new.id, new.name, new.signature, new.summary);
end;
create trigger if not exists symbols_ad after delete on symbols begin
  insert into symbols_fts(symbols_fts, rowid, name, signature, summary)
  values ('delete', old.id, old.name, old.signature, old.summary);
end;
create trigger if not exists symbols_au after update on symbols begin
  insert into symbols_fts(symbols_fts, rowid, name, signature, summary)
  values ('delete', old.id, old.name, old.signature, old.summary);
  insert into symbols_fts(rowid, name, signature, summary)
  values (new.id, new.name, new.signature, new.summary);
end;
"#;

/// `$XDG_DATA_HOME/panoptes/panoptes.db`, else `~/.local/share/panoptes/panoptes.db`.
pub fn default_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(x) if !x.is_empty() => PathBuf::from(x),
        _ => {
            let home = std::env::var_os("HOME").context("neither XDG_DATA_HOME nor HOME is set")?;
            PathBuf::from(home).join(".local").join("share")
        }
    };
    Ok(base.join("panoptes").join("panoptes.db"))
}

/// Open (creating if absent), enable WAL and foreign keys, and apply the schema.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    if crate::progress::enabled() {
        let mut steps = 0u64;
        db.progress_handler(
            50_000,
            Some(move || {
                steps = steps.saturating_add(50_000);
                crate::progress::database_steps(steps);
                false
            }),
        )?;
    }

    let found: i64 = db.query_row("pragma user_version", [], |r| r.get(0))?;
    anyhow::ensure!(
        matches!(found, 0..=SCHEMA_VERSION),
        "store at {} is schema v{found}; expected v{SCHEMA_VERSION}",
        path.display()
    );

    // Established stores only read pragmas: readers must not acquire schema
    // locks while another repository is being indexed.
    let mode: String = db.query_row(
        if found == 0 {
            "pragma journal_mode=wal"
        } else {
            "pragma journal_mode"
        },
        [],
        |r| r.get(0),
    )?;
    anyhow::ensure!(
        mode.eq_ignore_ascii_case("wal"),
        "journal_mode is {mode:?}, not wal"
    );
    db.execute_batch("pragma foreign_keys=on; pragma synchronous=normal;")?;
    if found < SCHEMA_VERSION {
        let tx =
            rusqlite::Transaction::new_unchecked(&db, rusqlite::TransactionBehavior::Immediate)
                .context("begin store migration")?;
        // Another process may have migrated between opening and taking the lock.
        let version: i64 = tx.query_row("pragma user_version", [], |r| r.get(0))?;
        anyhow::ensure!(
            matches!(version, 0..=SCHEMA_VERSION),
            "unsupported store schema v{version}"
        );
        if version == 0 {
            tx.execute_batch(DDL).context("apply schema")?;
        }
        if version < 3 {
            tx.execute_batch(
                "create index if not exists edges_fk_src on edges(src_symbol_id);
                 create index if not exists edges_fk_dst on edges(dst_symbol_id);
                 create table if not exists repo_inputs (
                   repo_id integer not null references repos(id) on delete cascade,
                   path text not null,
                   content blob,
                   primary key(repo_id, path)
                 ) without rowid;",
            )
            .context("index edge foreign keys and track resolver inputs")?;
        }
        if version < 4 {
            tx.execute_batch(crate::identity::DDL)
                .context("create repository identities")?;
            tx.execute(
                "insert into checkouts(root,graph_repo_id) select root,id from repos",
                [],
            )?;
        }
        if version < 5 {
            tx.execute_batch(crate::content::DDL)
                .context("create shared source and extraction cache")?;
        }
        if version < 2 {
            tx.execute_batch(crate::search::DDL)
                .context("create search term index")?;
            if version == 1 {
                crate::search::backfill(&tx).context("backfill search terms")?;
            }
        }
        if version < 6 {
            tx.execute_batch(crate::snapshot::DDL)
                .context("separate immutable snapshots from checkouts")?;
            let roots = {
                let mut statement = tx.prepare("select root,snapshot_id from checkouts")?;
                statement
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?
            };
            for (root, snapshot) in roots {
                if Path::new(&root).exists() {
                    crate::identity::register(&tx, Path::new(&root), snapshot)?;
                }
            }
        }
        if version < 7 {
            crate::snapshot::enable_reuse(&tx)?;
        }
        if version < SCHEMA_VERSION {
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        tx.commit().context("commit store migration")?;
    }
    Ok(db)
}

pub fn reset_repo(db: &Connection, root: &Path) -> Result<bool> {
    let tx = db.unchecked_transaction()?;
    let changed = tx.execute("update checkouts set snapshot_id=null,generation=generation+1 where root=?1 and snapshot_id is not null", [root.to_string_lossy()])?;
    crate::snapshot::collect_unreferenced(&tx)?;
    tx.commit()?;
    Ok(changed > 0)
}

/// Remove graphs and identities together, preserving a reusable store.
pub fn clear(db: &Connection) -> Result<i64> {
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)?;
    let count = tx.query_row(
        "select count(*) from checkouts where snapshot_id is not null",
        [],
        |row| row.get(0),
    )?;
    tx.execute("delete from checkouts", [])?;
    tx.execute("delete from snapshots", [])?;
    tx.execute("delete from git_instances", [])?;
    tx.execute("delete from lineages", [])?;
    tx.execute("delete from content_objects", [])?;
    tx.execute("delete from extraction_profiles", [])?;
    tx.commit()?;
    db.execute_batch("pragma wal_checkpoint(truncate); vacuum;")?;
    Ok(count)
}

pub fn prune_missing(db: &Connection) -> Result<Vec<String>> {
    let roots = {
        let mut statement =
            db.prepare("select root from checkouts where snapshot_id is not null order by root")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    let missing: Vec<_> = roots
        .into_iter()
        .filter(|root| !Path::new(root).exists())
        .collect();
    let tx = db.unchecked_transaction()?;
    for root in &missing {
        tx.execute(
            "update checkouts set snapshot_id=null,generation=generation+1 where root=?1",
            [root],
        )?;
    }
    crate::snapshot::collect_unreferenced(&tx)?;
    tx.commit()?;
    Ok(missing)
}

pub fn integrity(db: &Connection) -> Result<String> {
    db.query_row("pragma integrity_check", [], |row| row.get(0))
        .context("run SQLite integrity check")
}

/// Preserve a broken store beside the replacement. Recovery never destroys the
/// only copy an operator might need for diagnosis.
pub fn recover(path: &Path) -> Result<PathBuf> {
    anyhow::ensure!(path.exists(), "{} does not exist", path.display());
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let backup = path.with_extension(format!("corrupt-{timestamp}.db"));
    std::fs::rename(path, &backup)
        .with_context(|| format!("preserve corrupt store as {}", backup.display()))?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
        if sidecar.exists() {
            let backup_sidecar = PathBuf::from(format!("{}{suffix}", backup.display()));
            std::fs::rename(sidecar, backup_sidecar)?;
        }
    }
    open(path).context("create replacement store")?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (tempdir::Guard, Connection) {
        let g = tempdir::Guard::new();
        let db = open(&g.path().join("panoptes.db")).expect("open");
        (g, db)
    }

    /// Minimal scratch-dir helper so the tests need no dev-dependency.
    mod tempdir {
        use std::path::{Path, PathBuf};
        pub struct Guard(PathBuf);
        impl Guard {
            pub fn new() -> Self {
                // Monotonic counter + pid: unique per test without pulling in rand.
                use std::sync::atomic::{AtomicU32, Ordering};
                static N: AtomicU32 = AtomicU32::new(0);
                let p = std::env::temp_dir().join(format!(
                    "panoptes-test-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&p).unwrap();
                Guard(p)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    fn seed_repo(db: &Connection) -> i64 {
        let modern: bool = db
            .query_row(
                "select exists(select 1 from sqlite_master where name='snapshots')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        db.execute(if modern {
            "insert into snapshots(storage_key, git_common_dir, indexed_at, extractor_stamp) values ('/src/demo','/src/demo/.git',1,'stamp-a')"
        } else {
            "insert into repos(root, git_common_dir, indexed_at, extractor_stamp) values ('/src/demo','/src/demo/.git',1,'stamp-a')"
        }, []).unwrap();
        let repo = db.last_insert_rowid();
        if modern {
            db.execute(
                "insert into snapshot_manifests(snapshot_id,ready,source_complete) values (?1,1,0)",
                [repo],
            )
            .unwrap();
            crate::identity::register(db, Path::new("/src/demo"), Some(repo)).unwrap();
        }
        db.execute(
            "insert into files(repo_id, path, mtime, size, hash)
             values (?1, 'src/a.ts', 1, 10, 'h')",
            [repo],
        )
        .unwrap();
        let file = db.last_insert_rowid();
        db.execute(
            "insert into symbols(repo_id, file_id, name, kind, start_line, end_line, signature, summary)
             values (?1, ?2, 'parseRepo', 'function', 1, 9, 'fn parseRepo(p)', 'walks the tree')",
            [repo, file],
        )
        .unwrap();
        repo
    }

    #[test]
    fn open_is_idempotent_and_stamps_the_version() {
        let g = tempdir::Guard::new();
        let p = g.path().join("panoptes.db");
        let db = open(&p).unwrap();
        let v: i64 = db
            .query_row("pragma user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        drop(db);
        open(&p).expect("re-opening an existing store must not fail");
    }

    #[test]
    fn migrates_v1_search_terms_without_changing_the_snapshot() {
        let g = tempdir::Guard::new();
        let path = g.path().join("old.db");
        let old = Connection::open(&path).unwrap();
        old.execute_batch("pragma journal_mode=wal;").unwrap();
        old.execute_batch(DDL).unwrap();
        old.execute_batch("pragma user_version=1;").unwrap();
        let repo = seed_repo(&old);
        let before: i64 = old
            .query_row("select count(*) from symbols", [], |r| r.get(0))
            .unwrap();
        drop(old);
        let db = open(&path).unwrap();
        let after: i64 = db
            .query_row("select count(*) from symbols", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, after);
        let cached: i64 = db
            .query_row(
                "select count(distinct symbol_id) from search_terms where repo_id=?1",
                [repo],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cached, before);
        let terms: i64 = db
            .query_row("select count(*) from search_terms", [], |r| r.get(0))
            .unwrap();
        drop(db);
        let db = open(&path).unwrap();
        assert_eq!(
            terms,
            db.query_row("select count(*) from search_terms", [], |r| r
                .get::<_, i64>(0))
                .unwrap()
        );
        reset_repo(&db, Path::new("/src/demo")).unwrap();
        assert_eq!(
            0,
            db.query_row("select count(*) from search_terms", [], |r| r
                .get::<_, i64>(0))
                .unwrap()
        );
    }

    #[test]
    fn migration_indexes_each_edge_foreign_key_without_rewriting_graph() {
        let g = tempdir::Guard::new();
        let path = g.path().join("v2.db");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("pragma journal_mode=wal").unwrap();
        db.execute_batch(DDL).unwrap();
        db.execute_batch(crate::search::DDL).unwrap();
        db.execute_batch("pragma user_version=2").unwrap();
        let repo = seed_repo(&db);
        drop(db);
        let db = open(&path).unwrap();
        for (column, index) in [
            ("src_symbol_id", "edges_fk_src"),
            ("dst_symbol_id", "edges_fk_dst"),
        ] {
            let plan: String = db
                .query_row(
                    &format!("explain query plan select rowid from edges where {column}=?1"),
                    [1],
                    |row| row.get(3),
                )
                .unwrap();
            assert!(plan.contains("SEARCH") && plan.contains(index), "{plan}");
        }
        let count: i64 = db
            .query_row(
                "select count(*) from symbols where repo_id=?1",
                [repo],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(integrity(&db).unwrap(), "ok");
        assert!(
            db.prepare("pragma foreign_key_check")
                .unwrap()
                .query([])
                .unwrap()
                .next()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn snapshot_migration_preserves_v3_v4_v5_graphs_and_existing_checkout_ids() {
        for version in 3..=5 {
            let guard = tempdir::Guard::new();
            let path = guard.path().join("historical.db");
            let old = Connection::open(&path).unwrap();
            old.execute_batch("pragma journal_mode=wal").unwrap();
            old.execute_batch(DDL).unwrap();
            old.execute_batch(crate::search::DDL).unwrap();
            old.execute_batch("create index edges_fk_src on edges(src_symbol_id); create index edges_fk_dst on edges(dst_symbol_id); create table repo_inputs(repo_id integer not null references repos(id) on delete cascade,path text not null,content blob,primary key(repo_id,path)) without rowid;").unwrap();
            let graph = seed_repo(&old);
            let file: i64 = old
                .query_row("select id from files where repo_id=?1", [graph], |row| {
                    row.get(0)
                })
                .unwrap();
            old.execute("insert into file_extracts(file_id,extractor_stamp,payload) values (?1,'legacy','legacy-contextual-payload')",[file]).unwrap();
            if version >= 4 {
                old.execute_batch(crate::identity::DDL).unwrap();
                old.execute("insert into checkouts(uid,root,graph_repo_id) values ('0123456789abcdef0123456789abcdef','/src/demo',?1)",[graph]).unwrap();
            }
            if version >= 5 {
                old.execute_batch(crate::content::DDL).unwrap();
            }
            old.pragma_update(None, "user_version", version).unwrap();
            drop(old);
            let db = open(&path).unwrap();
            let (active, uid): (i64, String) = db
                .query_row(
                    "select snapshot_id,uid from checkouts where root='/src/demo'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(active, graph);
            if version >= 4 {
                assert_eq!(uid, "0123456789abcdef0123456789abcdef");
            }
            assert_eq!(
                db.query_row(
                    "select payload from file_extracts where file_id=?1",
                    [file],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
                "legacy-contextual-payload"
            );
            assert!(crate::snapshot::current_key(&db, graph).unwrap().is_none());
            assert_eq!(
                db.query_row("select count(*) from extractions", [], |row| row
                    .get::<_, i64>(0))
                    .unwrap(),
                0
            );
            assert!(
                db.prepare("pragma foreign_key_check")
                    .unwrap()
                    .query([])
                    .unwrap()
                    .next()
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn rejects_unrecognized_schema_versions() {
        let g = tempdir::Guard::new();
        let p = g.path().join("panoptes.db");
        let db = Connection::open(&p).unwrap();
        db.execute_batch("pragma user_version=99;").unwrap();
        drop(db);

        let error = open(&p).unwrap_err().to_string();
        assert!(
            error.contains(&format!("schema v99; expected v{SCHEMA_VERSION}")),
            "{error}"
        );
    }

    #[test]
    fn wal_reader_sees_committed_state_while_writer_is_open() {
        let g = tempdir::Guard::new();
        let path = g.path().join("panoptes.db");
        let mut writer = open(&path).unwrap();
        let reader = open(&path).unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute(
            "insert into snapshots(storage_key, indexed_at, extractor_stamp) values ('/pending', 1, 'x')",
            [],
        )
        .unwrap();
        let before: i64 = reader
            .query_row("select count(*) from snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(before, 0);
        tx.commit().unwrap();
        let after: i64 = reader
            .query_row("select count(*) from snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(after, 1);
    }

    #[test]
    fn opening_an_existing_store_does_not_contend_with_a_writer() {
        let g = tempdir::Guard::new();
        let path = g.path().join("panoptes.db");
        let mut writer = open(&path).unwrap();
        let tx = writer.transaction().unwrap();
        tx.execute(
            "insert into snapshots(storage_key, indexed_at, extractor_stamp) values ('/pending', 1, 'x')",
            [],
        )
        .unwrap();

        let reader = open(&path).unwrap();
        let visible: i64 = reader
            .query_row("select count(*) from snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(visible, 0, "the uncommitted writer remains invisible");
    }

    #[test]
    fn dropped_transaction_leaves_no_partial_repo() {
        let g = tempdir::Guard::new();
        let path = g.path().join("panoptes.db");
        let mut db = open(&path).unwrap();
        {
            let tx = db.transaction().unwrap();
            tx.execute(
                "insert into snapshots(storage_key, indexed_at, extractor_stamp) values ('/partial', 1, 'x')",
                [],
            )
            .unwrap();
        }
        let count: i64 = db
            .query_row("select count(*) from snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn fts_mirrors_symbol_inserts_updates_and_deletes() {
        // The external-content index is only correct while the triggers fire; if
        // they ever stop, searches keep returning rows that no longer exist.
        let (_g, db) = temp_db();
        seed_repo(&db);

        let hit = |q: &str| -> Vec<String> {
            db.prepare("select name from symbols_fts where symbols_fts match ?1")
                .unwrap()
                .query_map([q], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };

        assert_eq!(hit("parse*"), vec!["parseRepo".to_string()], "insert");

        // A match is against EVERY indexed column, so the update has to move the
        // token out of `signature` as well. Renaming `name` alone would leave
        // `fn parseRepo(p)` behind and `parse*` would still hit — correctly, which
        // makes that a useless assertion rather than a failing one.
        db.execute(
            "update symbols set name='renameRepo', signature='fn renameRepo(p)'
             where name='parseRepo'",
            [],
        )
        .unwrap();
        assert!(
            hit("parse*").is_empty(),
            "update must retract the old terms"
        );
        assert_eq!(hit("rename*"), vec!["renameRepo".to_string()], "update");

        db.execute("delete from symbols", []).unwrap();
        assert!(hit("rename*").is_empty(), "delete must retract the row");

        // The index and its content table must still agree; external-content FTS5
        // reports drift here rather than at query time, where it surfaces as the
        // far less obvious "database disk image is malformed".
        db.execute(
            "insert into symbols_fts(symbols_fts) values('integrity-check')",
            [],
        )
        .expect("fts index and content table disagree");
    }

    #[test]
    fn dropping_a_repo_cascades_to_files_symbols_edges_and_the_index() {
        let (_g, db) = temp_db();
        let repo = seed_repo(&db);
        let sym: i64 = db
            .query_row("select id from symbols", [], |r| r.get(0))
            .unwrap();
        db.execute(
            "insert into edges(repo_id, src_symbol_id, dst_symbol_id, kind)
             values (?1, ?2, ?2, 'calls')",
            [repo, sym],
        )
        .unwrap();

        reset_repo(&db, Path::new("/src/demo")).unwrap();

        for t in ["files", "symbols", "edges"] {
            let n: i64 = db
                .query_row(&format!("select count(*) from {t}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{t} should have cascaded away");
        }
        let n: i64 = db
            .query_row(
                "select count(*) from symbols_fts where symbols_fts match 'parse*'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "fts must not outlive the symbols it indexes");
    }

    #[test]
    fn clear_removes_all_graph_rows_and_keeps_the_store_usable() {
        let (_g, db) = temp_db();
        seed_repo(&db);
        assert_eq!(clear(&db).unwrap(), 1);
        for table in ["snapshots", "files", "file_extracts", "symbols", "edges"] {
            let rows: i64 = db
                .query_row(&format!("select count(*) from {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(rows, 0, "{table} should be empty");
        }
        assert_eq!(integrity(&db).unwrap(), "ok");
        seed_repo(&db);
    }

    #[test]
    fn failed_clear_rolls_back_graphs_and_identity_metadata_together() {
        let (_g, db) = temp_db();
        let graph = seed_repo(&db);
        db.execute("insert into git_instances default values", [])
            .unwrap();
        let instance = db.last_insert_rowid();
        db.execute(
            "update checkouts set instance_id=?1 where snapshot_id=?2",
            rusqlite::params![instance, graph],
        )
        .unwrap();
        db.execute_batch("create trigger reject_instance_delete before delete on git_instances begin select raise(abort,'injected clear failure'); end;").unwrap();
        assert!(clear(&db).is_err());
        assert_eq!(
            db.query_row(
                "select snapshot_id from checkouts where root='/src/demo'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            graph
        );
        assert!(
            db.query_row("select count(*) from symbols", [], |r| r.get::<_, i64>(0))
                .unwrap()
                > 0
        );
        assert_eq!(integrity(&db).unwrap(), "ok");
    }

    #[test]
    fn a_repo_root_can_only_be_registered_once() {
        let (_g, db) = temp_db();
        seed_repo(&db);
        let again = db.execute("insert into checkouts(root) values ('/src/demo')", []);
        assert!(
            again.is_err(),
            "checkouts.root is the selector; duplicates would make it ambiguous"
        );
    }
}
