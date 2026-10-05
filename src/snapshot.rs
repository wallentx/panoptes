//! Complete graph input manifests and atomic checkout attachment.
use crate::{
    index,
    repo::{self, SourceFile},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub const DDL: &str = r#"
alter table repos rename to snapshots;
alter table snapshots rename column root to storage_key;
create table snapshot_manifests (
 snapshot_id integer primary key references snapshots(id) on delete cascade,
 input_key text,
 source_tree_key text,
 manifest text,
 build_stats text,
 ready integer not null check(ready in (0,1)),
 source_complete integer not null check(source_complete in (0,1))
);
-- Existing graphs are private, unverified legacy snapshots. Never fabricate keys.
insert into snapshot_manifests(snapshot_id,ready,source_complete) select id,1,0 from snapshots;
create index snapshots_by_input on snapshot_manifests(input_key) where ready=1 and source_complete=1;
create table checkouts_new (
 id integer primary key,
 uid text not null unique default (lower(hex(randomblob(16)))),
 root text not null unique,
 instance_id integer references git_instances(id),
 snapshot_id integer references snapshots(id) on delete restrict,
 generation integer not null default 0,
 attached_at integer not null default 0
);
insert into checkouts_new(id,uid,root,instance_id,snapshot_id)
 select id,uid,root,instance_id,graph_repo_id from checkouts;
drop table checkouts;
alter table checkouts_new rename to checkouts;
create index checkouts_by_instance on checkouts(instance_id);
create index checkouts_by_snapshot on checkouts(snapshot_id);
create trigger attach_ready_snapshot_insert before insert on checkouts
 when new.snapshot_id is not null and not exists(select 1 from snapshot_manifests where snapshot_id=new.snapshot_id and ready=1)
 begin select raise(abort,'cannot attach an unfinished snapshot'); end;
create trigger attach_ready_snapshot_update before update of snapshot_id on checkouts
 when new.snapshot_id is not null and not exists(select 1 from snapshot_manifests where snapshot_id=new.snapshot_id and ready=1)
 begin select raise(abort,'cannot attach an unfinished snapshot'); end;
-- Preserve old graph rows and IDs while enforcing ownership on all new edges.
create trigger edges_same_snapshot_insert before insert on edges
 when not exists(select 1 from symbols where id=new.src_symbol_id and repo_id=new.repo_id)
   or not exists(select 1 from symbols where id=new.dst_symbol_id and repo_id=new.repo_id)
 begin select raise(abort,'edge endpoints belong to another snapshot'); end;
create trigger edges_same_snapshot_update before update on edges
 when not exists(select 1 from symbols where id=new.src_symbol_id and repo_id=new.repo_id)
   or not exists(select 1 from symbols where id=new.dst_symbol_id and repo_id=new.repo_id)
 begin select raise(abort,'edge endpoints belong to another snapshot'); end;
"#;

pub struct Manifest {
    pub key: String,
    pub tree_key: String,
    pub json: String,
}

pub fn manifest(files: &[SourceFile], go_mod: Option<&[u8]>) -> Result<Manifest> {
    #[derive(Serialize)]
    struct Inputs<'a> {
        format: &'static str,
        scan_policy: &'static str,
        extractor: &'static str,
        graph_version: u32,
        search_version: u32,
        files: Vec<(&'a str, &'static str, &'a str)>,
        go_mod: Option<String>,
    }
    let mut inputs = Inputs {
        format: "panoptes-graph-inputs-v1",
        scan_policy: "strict-utf8-sources-no-symlinks-v1",
        extractor: index::EXTRACTOR_STAMP,
        graph_version: 1,
        search_version: 1,
        files: files
            .iter()
            .map(|f| (f.rel.as_str(), f.lang.key(), f.hash.as_str()))
            .collect(),
        go_mod: go_mod.map(repo::content_hash),
    };
    inputs.files.sort_unstable();
    ensure!(
        inputs.files.windows(2).all(|pair| pair[0].0 != pair[1].0),
        "duplicate source path in snapshot input"
    );
    let tree: Vec<_> = inputs
        .files
        .iter()
        .map(|&(path, _, hash)| (path, hash))
        .collect();
    let tree_key = repo::content_hash(&serde_json::to_vec(&("panoptes-source-tree-v1", tree))?);
    let json = serde_json::to_string(&inputs)?;
    Ok(Manifest {
        key: repo::content_hash(json.as_bytes()),
        tree_key,
        json,
    })
}

pub fn begin(db: &Connection, manifest: &Manifest, go_mod: Option<&[u8]>, now: i64) -> Result<i64> {
    db.execute("insert into snapshots(storage_key,indexed_at,extractor_stamp) values (lower(hex(randomblob(16))),?1,?2)", params![now,index::EXTRACTOR_STAMP])?;
    let id = db.last_insert_rowid();
    db.execute("insert into snapshot_manifests(snapshot_id,input_key,source_tree_key,manifest,ready,source_complete) values (?1,?2,?3,?4,0,0)",params![id,manifest.key,manifest.tree_key,manifest.json])?;
    db.execute(
        "insert into repo_inputs(repo_id,path,content) values (?1,'go.mod',?2)",
        params![id, go_mod],
    )?;
    Ok(id)
}

pub fn finish(
    db: &Connection,
    id: i64,
    root: &std::path::Path,
    stats: &index::BuildStats,
) -> Result<()> {
    let missing: i64 = db.query_row("select count(*) from files f left join file_objects o on o.file_id=f.id where f.repo_id=?1 and o.file_id is null", [id], |row| row.get(0))?;
    ensure!(missing == 0, "snapshot source capture is incomplete");
    db.execute(
        "update snapshot_manifests set ready=1,source_complete=1,build_stats=?2 where snapshot_id=?1",
        params![id,serde_json::to_string(stats)?],
    )?;
    crate::identity::register(db, root, Some(id))?;
    Ok(())
}

pub fn current_key(db: &Connection, id: i64) -> Result<Option<String>> {
    db.query_row("select input_key from snapshot_manifests where snapshot_id=?1 and ready=1 and source_complete=1", [id], |row| row.get(0)).optional().map(Option::flatten).context("read snapshot input key")
}

/// Explicit cache reclamation only considers snapshots with no attached checkout.
pub fn collect_unreferenced(db: &Connection) -> Result<usize> {
    Ok(db.execute("delete from snapshots where not exists(select 1 from checkouts where snapshot_id=snapshots.id)", [])?)
}

pub fn reused_stats(db: &Connection, id: i64) -> Result<index::BuildStats> {
    let json: String = db.query_row(
        "select build_stats from snapshot_manifests where snapshot_id=?1 and ready=1",
        [id],
        |row| row.get(0),
    )?;
    let mut stats: index::BuildStats =
        serde_json::from_str(&json).context("read snapshot build summary")?;
    stats.parsed = 0;
    stats.reused = stats.files;
    stats.deleted = 0;
    Ok(stats)
}

/// Migrate verified v6 duplicates before enforcing one ready graph per input.
pub fn enable_reuse(db: &Connection) -> Result<()> {
    let conflicts: i64 = db.query_row("select count(*) from snapshot_manifests a join snapshot_manifests b on a.input_key=b.input_key and a.snapshot_id<b.snapshot_id where a.ready=1 and a.source_complete=1 and b.ready=1 and b.source_complete=1 and a.manifest is not b.manifest", [], |r| r.get(0))?;
    ensure!(
        conflicts == 0,
        "conflicting snapshot manifests share an input key"
    );
    db.execute_batch("update checkouts set snapshot_id=(select min(b.snapshot_id) from snapshot_manifests a join snapshot_manifests b on b.input_key=a.input_key where a.snapshot_id=checkouts.snapshot_id and b.ready=1 and b.source_complete=1), generation=generation+1, attached_at=unixepoch() where snapshot_id in (select a.snapshot_id from snapshot_manifests a where a.ready=1 and a.source_complete=1 and exists(select 1 from snapshot_manifests b where b.input_key=a.input_key and b.ready=1 and b.source_complete=1 and b.snapshot_id<a.snapshot_id));")?;
    collect_unreferenced(db)?;
    db.execute_batch("drop index snapshots_by_input; create unique index snapshots_by_input on snapshot_manifests(input_key) where ready=1 and source_complete=1;")?;
    Ok(())
}

pub fn lookup(db: &Connection, manifest: &Manifest) -> Result<Option<i64>> {
    let found: Option<(i64, String)> = db.query_row("select snapshot_id,manifest from snapshot_manifests m join snapshots s on s.id=m.snapshot_id where input_key=?1 and ready=1 and source_complete=1 and extractor_stamp=?2", params![manifest.key,index::EXTRACTOR_STAMP], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    if let Some((id, stored)) = found {
        ensure!(
            stored == manifest.json,
            "snapshot digest collision or corrupt manifest"
        );
        Ok(Some(id))
    } else {
        Ok(None)
    }
}

/// Transactional, explicit reclamation. Attached snapshots retain their sources.
pub fn gc(db: &mut Connection) -> Result<serde_json::Value> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let snapshots = collect_unreferenced(&tx)?;
    let objects = tx.execute("delete from content_objects where not exists(select 1 from file_objects where object_id=content_objects.id)", [])?;
    let profiles = tx.execute("delete from extraction_profiles where not exists(select 1 from extractions where profile_id=extraction_profiles.id)", [])?;
    tx.commit()?;
    Ok(
        serde_json::json!({"snapshots":snapshots,"sourceObjects":objects,"extractionProfiles":profiles}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, identity, index};
    use std::path::PathBuf;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "panoptes-sharing-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn root(&self, name: &str, source: &str) -> PathBuf {
            let root = self.0.join(name);
            std::fs::create_dir_all(&root).unwrap();
            if !source.is_empty() {
                std::fs::write(root.join("a.rs"), source).unwrap();
            }
            root
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn shared_graphs_diverge_reset_and_gc_without_damaging_other_checkouts() {
        let f = Fixture::new();
        let a = f.root("first", "pub fn original() {}\n");
        let b = f.root("second", "pub fn original() {}\n");
        let mut db = db::open(&f.0.join("store.db")).unwrap();
        index::build(&mut db, &a).unwrap();
        let counts = |db: &Connection| -> (i64, i64, i64) {
            db.query_row("select (select count(*) from symbols),(select count(*) from search_terms),(select count(*) from extractions)", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap()
        };
        let before = counts(&db);
        let stats = index::build(&mut db, &b).unwrap();
        assert_eq!(stats.parsed, 0);
        assert_eq!(stats.reused, 1);
        assert_eq!(counts(&db), before);
        let old = index::repo_id_of(&db, &a).unwrap().unwrap();
        assert_eq!(Some(old), index::repo_id_of(&db, &b).unwrap());
        let a_uid = identity::metadata(&db, &a).unwrap()["checkoutId"].clone();
        assert_ne!(a_uid, identity::metadata(&db, &b).unwrap()["checkoutId"]);
        std::fs::write(a.join("a.rs"), "pub fn changed() {}\n").unwrap();
        index::build(&mut db, &a).unwrap();
        assert_ne!(Some(old), index::repo_id_of(&db, &a).unwrap());
        assert_eq!(Some(old), index::repo_id_of(&db, &b).unwrap());
        assert!(
            crate::content::source(&db, old, "a.rs")
                .unwrap()
                .unwrap()
                .contains("original")
        );
        db::reset_repo(&db, &a).unwrap();
        assert_eq!(a_uid, identity::metadata(&db, &a).unwrap()["checkoutId"]);
        assert_eq!(gc(&mut db).unwrap()["sourceObjects"], 1);
        assert_eq!(Some(old), index::repo_id_of(&db, &b).unwrap());
        assert_eq!(counts(&db), before);
        std::fs::remove_dir_all(&a).unwrap();
        db::prune_missing(&db).unwrap();
        assert_eq!(Some(old), index::repo_id_of(&db, &b).unwrap());
        assert_eq!(db::integrity(&db).unwrap(), "ok");
    }

    #[test]
    fn v6_duplicate_ready_snapshots_consolidate_preserving_checkout_ids() {
        let f = Fixture::new();
        let a = f.root("a", "");
        let b = f.root("b", "");
        let path = f.0.join("store.db");
        let mut db = db::open(&path).unwrap();
        let stats = index::build(&mut db, &a).unwrap();
        let first = index::repo_id_of(&db, &a).unwrap().unwrap();
        db.execute_batch("drop index snapshots_by_input; create index snapshots_by_input on snapshot_manifests(input_key) where ready=1 and source_complete=1;").unwrap();
        let tx = db.transaction().unwrap();
        let manifest = manifest(&[], None).unwrap();
        let second = begin(&tx, &manifest, None, 0).unwrap();
        finish(&tx, second, &b, &stats).unwrap();
        tx.commit().unwrap();
        let uid = identity::metadata(&db, &b).unwrap()["checkoutId"].clone();
        db.pragma_update(None, "user_version", 6).unwrap();
        drop(db);
        let db = db::open(&path).unwrap();
        assert_eq!(index::repo_id_of(&db, &a).unwrap(), Some(first));
        assert_eq!(index::repo_id_of(&db, &b).unwrap(), Some(first));
        assert_eq!(identity::metadata(&db, &b).unwrap()["checkoutId"], uid);
        assert_eq!(
            db.query_row("select count(*) from snapshots", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(db::integrity(&db).unwrap(), "ok");
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
