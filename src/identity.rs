//! Store-local identities and optional, bounded Git ancestry observations.
//! Paths locate instances/checkouts; neither paths nor ancestry roots are cache keys.
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const DDL: &str = r#"
create table if not exists git_instances (
 id integer primary key,
 uid text not null unique default (lower(hex(randomblob(16))))
);
create table if not exists git_instance_locations (
 instance_id integer primary key references git_instances(id) on delete cascade,
 common_dir text not null unique,
 filesystem_key text
);
create table if not exists checkouts (
 id integer primary key,
 uid text not null unique default (lower(hex(randomblob(16)))),
 root text not null unique,
 instance_id integer references git_instances(id),
 graph_repo_id integer unique references repos(id) on delete set null
);
create index if not exists checkouts_by_instance on checkouts(instance_id);
create table if not exists lineages (
 object_format text not null check(object_format in ('sha1','sha256')),
 root_oid text not null,
 primary key(object_format,root_oid),
 check(length(root_oid)=case object_format when 'sha1' then 40 else 64 end)
) without rowid;
create table if not exists ancestry_observations (
 id integer primary key,
 instance_id integer not null references git_instances(id) on delete cascade,
 head_oid text not null,
 object_format text not null,
 policy_stamp text not null,
 status text not null,
 observed_at integer not null,
 unique(instance_id,head_oid,policy_stamp)
);
create table if not exists ancestry_roots (
 observation_id integer not null references ancestry_observations(id) on delete cascade,
 object_format text not null,
 root_oid text not null,
 primary key(observation_id,object_format,root_oid),
 foreign key(object_format,root_oid) references lineages(object_format,root_oid)
) without rowid;
create index if not exists ancestry_roots_by_lineage on ancestry_roots(object_format,root_oid,observation_id);
"#;

// Only distinguishes a replaced locator on this machine; never exported as an ID
// and never used to infer continuity at another path.
fn filesystem_key(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(path).ok()?;
        Some(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn instance(db: &Connection, root: &Path) -> Result<Option<i64>> {
    let Some(common) = crate::repo::git_common_dir(root) else {
        return Ok(None);
    };
    let key = filesystem_key(Path::new(&common));
    let previous: Option<(i64, Option<String>)> = db
        .query_row(
            "select instance_id,filesystem_key from git_instance_locations where common_dir=?1",
            [&common],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((id, previous_key)) = previous {
        if previous_key.is_none() || previous_key == key {
            db.execute(
                "update git_instance_locations set filesystem_key=?1 where instance_id=?2",
                params![key, id],
            )?;
            return Ok(Some(id));
        }
        // The old instance remains distinct, even if its location was replaced.
        db.execute(
            "delete from git_instance_locations where instance_id=?1",
            [id],
        )?;
    }
    db.execute("insert into git_instances default values", [])?;
    let id = db.last_insert_rowid();
    db.execute("insert into git_instance_locations(instance_id,common_dir,filesystem_key) values (?1,?2,?3)", params![id,common,key])?;
    Ok(Some(id))
}

/// Caller owns the transaction. Explicit registration does not create a graph.
pub fn register(db: &Connection, root: &Path, graph: Option<i64>) -> Result<()> {
    let root_text = root.to_str().context("checkout path is not UTF-8")?;
    let instance = instance(db, root)?;
    db.execute(
        "insert into checkouts(root,instance_id,snapshot_id) values (?1,?2,?3)
         on conflict(root) do update set instance_id=excluded.instance_id,
           snapshot_id=coalesce(excluded.snapshot_id,checkouts.snapshot_id),
           generation=checkouts.generation+case when excluded.snapshot_id is not null and excluded.snapshot_id is not checkouts.snapshot_id then 1 else 0 end,
           attached_at=case when excluded.snapshot_id is not null and excluded.snapshot_id is not checkouts.snapshot_id then unixepoch() else checkouts.attached_at end",
        params![root_text, instance, graph],
    )?;
    Ok(())
}

pub fn metadata(db: &Connection, root: &Path) -> Result<Value> {
    let mut statement = db.prepare("select c.uid,i.uid,l.common_dir,l.filesystem_key,c.snapshot_id,m.input_key,m.source_complete,c.generation from checkouts c left join git_instances i on i.id=c.instance_id left join git_instance_locations l on l.instance_id=i.id left join snapshot_manifests m on m.snapshot_id=c.snapshot_id where c.root=?1")?;
    let mut rows = statement.query([root.to_string_lossy()])?;
    let Some(row) = rows.next()? else {
        return Ok(json!({"checkoutId":null,"gitInstanceId":null}));
    };
    let checkout: String = row.get(0)?;
    let mut instance: Option<String> = row.get(1)?;
    let common: Option<String> = row.get(2)?;
    let key: Option<String> = row.get(3)?;
    let snapshot: Option<i64> = row.get(4)?;
    let snapshot_key: Option<String> = row.get(5)?;
    let source_complete: Option<bool> = row.get(6)?;
    let generation: i64 = row.get(7)?;
    let live = crate::repo::git_common_dir(root);
    if common != live
        || common
            .as_ref()
            .is_some_and(|path| key != filesystem_key(Path::new(path)))
    {
        // Observational queries must not publish a stale instance after .git
        // replacement, nor acquire a writer lock just to repair metadata.
        instance = None;
    }
    Ok(
        json!({"checkoutId":checkout,"gitInstanceId":instance,"snapshotId":snapshot,"snapshotKey":snapshot_key,"sourceComplete":source_complete,"generation":generation}),
    )
}

pub fn inspect(db: &mut Connection, root: &Path, lineage: bool) -> Result<Value> {
    let target = crate::repo::checkout_target(root)?;
    let ancestry = lineage.then(|| ancestry(&target));
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    register(&tx, &target.root, None)?;
    let mut output = crate::repo::checkout_identity(&target);
    output
        .as_object_mut()
        .unwrap()
        .extend(metadata(&tx, &target.root)?.as_object().unwrap().clone());
    if let Some(ancestry) = ancestry {
        persist_ancestry(&tx, &target.root, &ancestry)?;
        output["ancestry"] = ancestry;
    }
    tx.commit()?;
    Ok(output)
}

const POLICY: &str = "physical-head-roots-v1";

/// No network, replace refs, all-refs walk, or unbounded child process.
fn git_output(root: &Path, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|key| key.starts_with("GIT_")) {
            command.env_remove(key);
        }
    }
    command
        .arg("--no-replace-objects")
        .arg("--no-lazy-fetch")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    bounded_output(command, Duration::from_secs(2))
}

fn bounded_output(mut command: Command, timeout: Duration) -> Result<Option<Vec<u8>>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let pipe = child.stdout.take().context("Git stdout missing")?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.take(128 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(Some(status)),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error);
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break Ok(None);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("Git output reader failed"))??;
    match status? {
        None => Ok(None),
        Some(status) => {
            ensure!(
                status.success() && bytes.len() <= 128 * 1024,
                "Git ancestry unavailable or too large"
            );
            Ok(Some(bytes))
        }
    }
}

fn ancestry(target: &crate::repo::Target) -> Value {
    let identity = crate::repo::checkout_identity(target);
    let mut result = json!({"policy":POLICY,"head":identity["head"],"objectFormat":null,"status":"unavailable","roots":[]});
    let Some(common) = crate::repo::git_common_dir(&target.root) else {
        return result;
    };
    let common = Path::new(&common);
    if let Some(head) = identity["head"].as_str() {
        result["objectFormat"] = if head.len() == 40 { "sha1" } else { "sha256" }.into();
    }
    // Grafts change the meaning of a root. Shallow roots are truncation points,
    // not verified genesis commits. Neither is promoted to lineage membership.
    for (path, status) in [
        ("info/grafts", "unsupported-grafts"),
        ("shallow", "shallow"),
    ] {
        match std::fs::read(common.join(path)) {
            Ok(bytes) if !bytes.is_empty() => {
                result["status"] = status.into();
                return result;
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return result;
            }
            _ => {}
        }
    }
    let head = match git_output(&target.root, &["rev-parse", "--verify", "HEAD"]) {
        Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).trim().to_string(),
        Ok(None) => {
            result["status"] = "timeout".into();
            return result;
        }
        Err(_) => {
            result["status"] = "unborn-or-unavailable".into();
            return result;
        }
    };
    if !matches!(head.len(), 40 | 64) || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return result;
    }
    let format = if head.len() == 40 { "sha1" } else { "sha256" };
    result["head"] = head.clone().into();
    result["objectFormat"] = format.into();
    crate::progress::report("Inspecting ancestry", 0, None, &head);
    match git_output(&target.root, &["rev-list", "--max-parents=0", &head]) {
        Ok(Some(bytes)) => {
            let text = String::from_utf8_lossy(&bytes);
            let mut roots: Vec<_> = text.lines().map(str::to_string).collect();
            if roots.is_empty()
                || roots.iter().any(|oid| {
                    oid.len() != head.len() || !oid.bytes().all(|b| b.is_ascii_hexdigit())
                })
            {
                return result;
            }
            // A concurrent shallow-boundary change must not promote fake roots.
            if common.join("shallow").exists() {
                result["status"] = "shallow".into();
                return result;
            }
            if std::fs::metadata(common.join("info/grafts"))
                .is_ok_and(|metadata| metadata.len() > 0)
            {
                result["status"] = "unsupported-grafts".into();
                return result;
            }
            roots.sort();
            roots.dedup();
            result["roots"] = json!(roots);
            result["status"] = "complete".into();
        }
        Ok(None) => result["status"] = "timeout".into(),
        Err(_) => {}
    }
    result
}

fn persist_ancestry(db: &Connection, root: &Path, observation: &Value) -> Result<()> {
    let Some(head) = observation["head"].as_str() else {
        return Ok(());
    };
    let Some(format) = observation["objectFormat"].as_str() else {
        return Ok(());
    };
    let instance: Option<i64> = db.query_row(
        "select instance_id from checkouts where root=?1",
        [root.to_string_lossy()],
        |row| row.get(0),
    )?;
    let Some(instance) = instance else {
        return Ok(());
    };
    db.execute("insert into ancestry_observations(instance_id,head_oid,object_format,policy_stamp,status,observed_at) values (?1,?2,?3,?4,?5,unixepoch()) on conflict(instance_id,head_oid,policy_stamp) do update set status=excluded.status,observed_at=excluded.observed_at", params![instance,head,format,POLICY,observation["status"].as_str()])?;
    let id: i64 = db.query_row("select id from ancestry_observations where instance_id=?1 and head_oid=?2 and policy_stamp=?3", params![instance,head,POLICY], |row| row.get(0))?;
    db.execute("delete from ancestry_roots where observation_id=?1", [id])?;
    if observation["status"] == "complete" {
        for oid in observation["roots"]
            .as_array()
            .context("ancestry roots missing")?
        {
            let oid = oid.as_str().context("invalid root")?;
            db.execute(
                "insert or ignore into lineages(object_format,root_oid) values (?1,?2)",
                params![format, oid],
            )?;
            db.execute("insert into ancestry_roots(observation_id,object_format,root_oid) values (?1,?2,?3)", params![id,format,oid])?;
        }
    }
    Ok(())
}

/// Explicit relocation preserves the UID. Content/HEAD similarity never does.
pub fn relocate(db: &mut Connection, from: &Path, to: &Path) -> Result<Value> {
    ensure!(
        !from.exists(),
        "old checkout still exists; relocation requires the old location to be absent"
    );
    let target = crate::repo::checkout_target(to)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (checkout, instance): (i64, Option<i64>) = tx
        .query_row(
            "select id,instance_id from checkouts where root=?1",
            [from.to_string_lossy()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .context("old checkout is not registered; use its stored absolute root")?;
    ensure!(
        tx.query_row(
            "select count(*) from checkouts where root=?1",
            [target.root.to_string_lossy()],
            |row| row.get::<_, i64>(0)
        )? == 0,
        "destination checkout is already registered"
    );
    if let (Some(instance), Some(common)) = (instance, crate::repo::git_common_dir(&target.root)) {
        let old_common: Option<String> = tx
            .query_row(
                "select common_dir from git_instance_locations where instance_id=?1",
                [instance],
                |row| row.get(0),
            )
            .optional()?;
        if old_common.as_deref() != Some(&common) {
            ensure!(
                old_common
                    .as_ref()
                    .is_none_or(|path| !Path::new(path).exists()),
                "old Git instance still exists; refusing to merge independent instances"
            );
            tx.execute(
                "delete from git_instance_locations where instance_id=?1",
                [instance],
            )?;
            tx.execute("insert into git_instance_locations(instance_id,common_dir,filesystem_key) values (?1,?2,?3)", params![instance,common,filesystem_key(Path::new(&common))])?;
        }
    }
    tx.execute(
        "update checkouts set root=?1 where id=?2",
        params![target.root.to_string_lossy(), checkout],
    )?;
    let result = metadata(&tx, &target.root)?;
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "panoptes-identities-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(root.join("repo")).unwrap();
            let fixture = Self(root.canonicalize().unwrap());
            git(&fixture.0.join("repo"), &["init", "-q"]);
            fixture
        }
        fn repo(&self) -> PathBuf {
            self.0.join("repo")
        }
        fn db(&self) -> Connection {
            crate::db::open(&self.0.join("store.db")).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=Panoptes test fixture",
                "-c",
                "user.email=tests@panoptes.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }
    fn commit(root: &Path, message: &str) -> String {
        git(root, &["commit", "--allow-empty", "-qm", message]);
        git(root, &["rev-parse", "HEAD"])
    }

    #[test]
    fn worktrees_share_an_instance_but_clones_do_not() {
        let fixture = Fixture::new();
        let mut db = fixture.db();
        commit(&fixture.repo(), "Initial independent history");
        let sibling = fixture.0.join("sibling");
        git(
            &fixture.repo(),
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                sibling.to_str().unwrap(),
            ],
        );
        let clone = fixture.0.join("clone");
        git(
            &fixture.repo(),
            &[
                "clone",
                "-q",
                "--shared",
                fixture.repo().to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        let a = inspect(&mut db, &fixture.repo(), true).unwrap();
        let b = inspect(&mut db, &sibling, true).unwrap();
        let c = inspect(&mut db, &clone, true).unwrap();
        assert_ne!(a["checkoutId"], b["checkoutId"]);
        assert_eq!(a["gitInstanceId"], b["gitInstanceId"]);
        assert_ne!(a["gitInstanceId"], c["gitInstanceId"]);
        assert_eq!(a["ancestry"]["roots"], c["ancestry"]["roots"]);
        assert_eq!(a["ancestry"]["status"], "complete");
        let again = inspect(&mut db, &fixture.repo(), false).unwrap();
        assert_eq!(a["checkoutId"], again["checkoutId"]);
        assert_eq!(
            db.query_row("select count(*) from snapshots", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn explicit_move_preserves_ids_without_merging_a_clone() {
        let fixture = Fixture::new();
        let mut db = fixture.db();
        let before = inspect(&mut db, &fixture.repo(), false).unwrap();
        let moved = fixture.0.join("moved");
        assert!(relocate(&mut db, &fixture.repo(), &fixture.repo()).is_err());
        std::fs::rename(fixture.repo(), &moved).unwrap();
        let after = relocate(&mut db, &fixture.repo(), &moved).unwrap();
        assert_eq!(before["checkoutId"], after["checkoutId"]);
        assert_eq!(before["gitInstanceId"], after["gitInstanceId"]);
        let again = inspect(&mut db, &moved, false).unwrap();
        assert_eq!(after["gitInstanceId"], again["gitInstanceId"]);
    }

    #[test]
    fn lineage_keeps_multiple_physical_roots_and_rejects_incomplete_history() {
        let fixture = Fixture::new();
        let mut db = fixture.db();
        let unborn = inspect(&mut db, &fixture.repo(), true).unwrap();
        assert_eq!(unborn["ancestry"]["roots"], json!([]));
        let a = commit(&fixture.repo(), "First history root");
        git(
            &fixture.repo(),
            &["checkout", "--orphan", "independent-history"],
        );
        let b = commit(&fixture.repo(), "Second history root");
        git(
            &fixture.repo(),
            &[
                "merge",
                "--allow-unrelated-histories",
                "-m",
                "Join independent histories",
                &a,
            ],
        );
        let observed = inspect(&mut db, &fixture.repo(), true).unwrap();
        let mut roots = vec![a.clone(), b];
        roots.sort();
        assert_eq!(observed["ancestry"]["roots"], json!(roots));
        let head = git(&fixture.repo(), &["rev-parse", "HEAD"]);
        // A replacement must not turn the merged physical history into one root.
        git(&fixture.repo(), &["replace", &head, &a]);
        assert_eq!(
            inspect(&mut db, &fixture.repo(), true).unwrap()["ancestry"]["roots"],
            json!(roots)
        );
        std::fs::write(fixture.repo().join(".git/shallow"), format!("{head}\n")).unwrap();
        let shallow = inspect(&mut db, &fixture.repo(), true).unwrap();
        assert_eq!(shallow["ancestry"]["status"], "shallow");
        assert_eq!(shallow["ancestry"]["roots"], json!([]));
        assert_eq!(
            db.query_row("select count(*) from ancestry_roots", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        std::fs::remove_file(fixture.repo().join(".git/shallow")).unwrap();
        std::fs::write(fixture.repo().join(".git/info/grafts"), format!("{head}\n")).unwrap();
        assert_eq!(
            inspect(&mut db, &fixture.repo(), true).unwrap()["ancestry"]["status"],
            "unsupported-grafts"
        );
    }

    #[test]
    fn replaced_git_directory_creates_a_new_instance_at_the_same_locator() {
        let fixture = Fixture::new();
        let mut db = fixture.db();
        let before = inspect(&mut db, &fixture.repo(), false).unwrap();
        std::fs::rename(fixture.repo().join(".git"), fixture.0.join("previous-git")).unwrap();
        git(&fixture.repo(), &["init", "-q"]);
        assert!(metadata(&db, &fixture.repo()).unwrap()["gitInstanceId"].is_null());
        let after = inspect(&mut db, &fixture.repo(), false).unwrap();
        assert_eq!(before["checkoutId"], after["checkoutId"]);
        assert_ne!(before["gitInstanceId"], after["gitInstanceId"]);
    }
    #[test]
    fn sha256_roots_keep_their_object_format() {
        let fixture = Fixture::new();
        let mut db = fixture.db();
        let repo = fixture.0.join("sha256");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "--object-format=sha256"]);
        let root = commit(&repo, "SHA-256 fixture history");
        let value = inspect(&mut db, &repo, true).unwrap();
        assert_eq!(root.len(), 64);
        assert_eq!(value["ancestry"]["objectFormat"], "sha256");
        assert_eq!(value["ancestry"]["roots"], json!([root]));
    }

    #[test]
    #[ignore = "subprocess fixture for bounded ancestry execution"]
    fn blocked_git_fixture() {
        std::thread::sleep(Duration::from_secs(5));
    }

    #[test]
    fn bounded_ancestry_stops_and_reaps_a_nonresponsive_child() {
        let mut command = crate::executable::command().unwrap();
        command.args([
            "--exact",
            "identity::tests::blocked_git_fixture",
            "--ignored",
            "--nocapture",
        ]);
        let started = Instant::now();
        assert!(
            bounded_output(command, Duration::from_millis(100))
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
