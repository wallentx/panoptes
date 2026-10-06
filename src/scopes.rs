//! Explicit query membership. Lineage is one observed root, never a transitive union.
use crate::{identity, repo};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub struct Expansion {
    pub targets: Vec<repo::Target>,
    pub unknown: usize,
    pub identities: HashMap<std::path::PathBuf, Value>,
}

pub fn validate(scope: &str, lineage: Option<&str>, tool: &str) -> Result<()> {
    ensure!(
        matches!(scope, "checkout" | "repository" | "lineage"),
        "scope must be checkout, repository, or lineage"
    );
    ensure!(
        scope == "checkout" || !matches!(tool, "freshness" | "worktrees"),
        "{tool} only supports checkout scope"
    );
    if scope == "lineage" {
        let (format, oid) = lineage.and_then(|s| s.split_once(':')).ok_or_else(|| {
            anyhow::anyhow!(
                "lineage scope requires lineageRoot as sha1:<root-oid> or sha256:<root-oid>"
            )
        })?;
        ensure!(
            (format == "sha1" && oid.len() == 40 || format == "sha256" && oid.len() == 64)
                && oid
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid lineageRoot"
        );
    } else {
        ensure!(lineage.is_none(), "lineageRoot requires lineage scope");
    }
    Ok(())
}

/// None means no currently usable ancestry observation; false is a verified nonmember.
fn lineage_member(db: &Connection, target: &repo::Target, lineage: &str) -> Result<Option<bool>> {
    let live = repo::checkout_identity(target);
    let metadata = identity::metadata(db, &target.root)?;
    let (Some(instance), Some(head), Some(common)) = (
        metadata["gitInstanceId"].as_str(),
        live["head"].as_str(),
        live["gitCommonDir"].as_str(),
    ) else {
        return Ok(None);
    };
    let common = std::path::Path::new(common);
    match std::fs::symlink_metadata(common.join("shallow")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        _ => return Ok(None),
    }
    let grafts = common.join("info/grafts");
    match std::fs::symlink_metadata(&grafts) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => match std::fs::metadata(&grafts) {
            Ok(metadata) if metadata.is_file() && metadata.len() == 0 => {}
            _ => return Ok(None),
        },
        Err(_) => return Ok(None),
    }
    let observation: Option<i64> = db.query_row("select o.id from ancestry_observations o join git_instances i on i.id=o.instance_id where i.uid=?1 and o.head_oid=?2 and o.policy_stamp='physical-head-roots-v1' and o.status='complete'", params![instance,head], |r| r.get(0)).optional()?;
    let Some(observation) = observation else {
        return Ok(None);
    };
    let (format, root) = lineage.split_once(':').expect("validated lineage");
    Ok(Some(db.query_row("select exists(select 1 from ancestry_roots where observation_id=?1 and object_format=?2 and root_oid=?3)", params![observation,format,root], |r| r.get(0))?))
}

pub fn expand(
    db: &Connection,
    selected: &[repo::Target],
    scope: &str,
    lineage: Option<&str>,
) -> Result<Expansion> {
    let mut instances = HashSet::new();
    for target in selected {
        if scope == "repository" {
            let metadata = identity::metadata(db, &target.root)?;
            let uid = metadata["gitInstanceId"].as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "{} has no verified registered Git instance",
                    target.root.display()
                )
            })?;
            instances.insert(uid.to_string());
        } else {
            ensure!(
                lineage_member(db, target, lineage.unwrap())? == Some(true),
                "{} lacks a current complete observation for this lineage; run panoptes identity {} --lineage",
                target.root.display(),
                target.root.display()
            );
        }
    }
    let mut statement = db.prepare("select root from checkouts order by root")?;
    let roots = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut output = Expansion {
        targets: Vec::new(),
        unknown: 0,
        identities: HashMap::new(),
    };
    for root in roots {
        let Ok(target) = repo::checkout_target(std::path::Path::new(&root)) else {
            continue;
        };
        // A deleted checkout nested under a surviving parent must not alias it.
        if target.root != std::path::Path::new(&root) {
            continue;
        }
        let eligible = if scope == "repository" {
            let metadata = identity::metadata(db, &target.root)?;
            metadata["gitInstanceId"]
                .as_str()
                .map(|uid| instances.contains(uid))
        } else {
            lineage_member(db, &target, lineage.unwrap())?
        };
        match eligible {
            Some(true) => {
                output
                    .identities
                    .insert(target.root.clone(), repo::checkout_identity(&target));
                output.targets.push(target);
            }
            Some(false) => {}
            None => output.unknown += 1,
        }
    }
    ensure!(
        !output.targets.is_empty(),
        "no registered live checkouts match the requested scope"
    );
    Ok(output)
}

pub fn still_member(
    db: &Connection,
    target: &repo::Target,
    scope: &str,
    lineage: Option<&str>,
    selected_instances: &HashSet<String>,
) -> Result<bool> {
    if scope == "lineage" {
        Ok(lineage_member(db, target, lineage.unwrap())? == Some(true))
    } else {
        Ok(identity::metadata(db, &target.root)?["gitInstanceId"]
            .as_str()
            .is_some_and(|uid| selected_instances.contains(uid)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, index};

    #[test]
    fn lineage_scope_requires_current_proof_and_does_not_union_unrelated_roots() {
        let temp =
            std::env::temp_dir().join(format!("panoptes-lineage-scope-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        let mut db = db::open(&temp.join("store.db")).unwrap();
        let root_a = "a".repeat(40);
        let root_b = "b".repeat(40);
        let mut targets = Vec::new();
        for (name, head, roots) in [
            ("a", "1".repeat(40), vec![root_a.clone()]),
            (
                "merged",
                "2".repeat(40),
                vec![root_a.clone(), root_b.clone()],
            ),
            ("b", "3".repeat(40), vec![root_b.clone()]),
            ("unknown", "4".repeat(40), vec![]),
        ] {
            let root = temp.join(name);
            std::fs::create_dir_all(root.join(".git")).unwrap();
            std::fs::write(root.join(".git/HEAD"), format!("{head}\n")).unwrap();
            std::fs::write(root.join("a.rs"), "pub fn shared() {}\n").unwrap();
            let root = root.canonicalize().unwrap();
            index::build(&mut db, &root).unwrap();
            let instance: i64 = db
                .query_row(
                    "select instance_id from checkouts where root=?1",
                    [root.to_str().unwrap()],
                    |r| r.get(0),
                )
                .unwrap();
            if !roots.is_empty() {
                db.execute("insert into ancestry_observations(instance_id,head_oid,object_format,policy_stamp,status,observed_at) values (?1,?2,'sha1','physical-head-roots-v1','complete',0)",params![instance,head]).unwrap();
                let observation = db.last_insert_rowid();
                for oid in roots {
                    db.execute("insert or ignore into lineages values ('sha1',?1)", [&oid])
                        .unwrap();
                    db.execute(
                        "insert into ancestry_roots values (?1,'sha1',?2)",
                        params![observation, oid],
                    )
                    .unwrap();
                }
            }
            targets.push(repo::checkout_target(&root).unwrap());
        }
        let lineage = format!("sha1:{root_a}");
        validate("lineage", Some(&lineage), "find").unwrap();
        let members = expand(&db, &targets[..1], "lineage", Some(&lineage)).unwrap();
        assert_eq!(
            members
                .targets
                .iter()
                .map(|t| t.label.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "merged"]
        );
        assert_eq!(members.unknown, 1);
        assert!(expand(&db, &targets[2..3], "lineage", Some(&lineage)).is_err());
        let store = temp.join("store.db");
        let output = crate::mcp::call_tool(
            &store,
            &targets[..1],
            "find",
            &serde_json::json!({"query":"shared","scope":"lineage","lineageRoot":lineage}),
            true,
        )
        .unwrap();
        assert_eq!(output["panoptesScope"]["snapshotsSearched"], 1);
        assert_eq!(output["panoptesCheckouts"].as_array().unwrap().len(), 2);
        #[cfg(unix)]
        for path in ["shallow", "info/grafts"] {
            let path = targets[1].root.join(".git").join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&path, &path).unwrap();
            let members = expand(&db, &targets[..1], "lineage", Some(&lineage)).unwrap();
            assert_eq!(
                members.targets.len(),
                1,
                "unverifiable boundary must exclude merged checkout"
            );
            assert_eq!(members.unknown, 2);
            std::fs::remove_file(path).unwrap();
        }
        std::fs::write(
            targets[1].root.join(".git/HEAD"),
            format!("{}\n", "5".repeat(40)),
        )
        .unwrap();
        let members = expand(&db, &targets[..1], "lineage", Some(&lineage)).unwrap();
        assert_eq!(members.targets.len(), 1);
        assert_eq!(members.unknown, 2);
        std::fs::write(targets[0].root.join(".git/shallow"), "boundary\n").unwrap();
        assert!(expand(&db, &targets[..1], "lineage", Some(&lineage)).is_err());
        drop(db);
        let _ = std::fs::remove_dir_all(temp);
    }
}
