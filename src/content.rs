//! Immutable source objects and base extraction artifacts. Context overlays stay
//! checkout-local; legacy file_extracts payloads are never promoted into this cache.
use crate::{extract, index, repo::SourceFile};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

pub const DDL: &str = r#"
create table if not exists content_objects (
 id integer primary key,
 digest text not null unique check(length(digest)=75 and substr(digest,1,11)='blake3-256:'),
 source_bytes blob not null,
 byte_length integer not null check(byte_length=length(source_bytes))
);
create table if not exists extraction_profiles (
 id integer primary key,
 extractor_stamp text not null,
 payload_schema integer not null,
 language text not null,
 relative_path text not null,
 mode text not null check(mode='base'),
 unique(extractor_stamp,payload_schema,language,relative_path,mode)
);
create table if not exists extractions (
 object_id integer not null references content_objects(id) on delete cascade,
 profile_id integer not null references extraction_profiles(id),
 payload text not null,
 primary key(object_id,profile_id)
) without rowid;
create index if not exists extractions_by_profile on extractions(profile_id);
create table if not exists file_objects (
 file_id integer primary key references files(id) on delete cascade,
 object_id integer not null references content_objects(id)
);
create index if not exists file_objects_by_object on file_objects(object_id);
"#;

pub fn put(db: &Connection, digest: &str, bytes: &[u8]) -> Result<i64> {
    db.execute("insert into content_objects(digest,source_bytes,byte_length) values (?1,?2,?3) on conflict(digest) do nothing", params![digest,bytes,bytes.len() as i64])?;
    let (id, identical): (i64, bool) = db.query_row(
        "select id,source_bytes=?2 from content_objects where digest=?1",
        params![digest, bytes],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        identical,
        "content digest collision or corrupt stored source object"
    );
    Ok(id)
}

pub fn load_base(db: &Connection, file: &SourceFile) -> Result<Option<extract::Extracted>> {
    let row: Option<(String, bool)> = db
        .query_row(
            "select e.payload,c.source_bytes=?5 from extractions e
         join content_objects c on c.id=e.object_id
         join extraction_profiles p on p.id=e.profile_id
         where c.digest=?1 and p.extractor_stamp=?2 and p.language=?3
           and p.relative_path=?4 and p.payload_schema=1 and p.mode='base'",
            params![
                file.hash,
                index::EXTRACTOR_STAMP,
                file.lang.key(),
                file.rel,
                file.text.as_bytes()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(payload, identical)| {
        ensure!(
            identical,
            "shared extraction source object does not match its digest"
        );
        serde_json::from_str(&payload).context("decode shared base extraction")
    })
    .transpose()
}

pub fn store_base(
    db: &Connection,
    file: &SourceFile,
    extracted: &extract::Extracted,
) -> Result<()> {
    let object = put(db, &file.hash, file.text.as_bytes())?;
    db.execute("insert or ignore into extraction_profiles(extractor_stamp,payload_schema,language,relative_path,mode) values (?1,1,?2,?3,'base')", params![index::EXTRACTOR_STAMP,file.lang.key(),file.rel])?;
    let profile: i64 = db.query_row("select id from extraction_profiles where extractor_stamp=?1 and payload_schema=1 and language=?2 and relative_path=?3 and mode='base'", params![index::EXTRACTOR_STAMP,file.lang.key(),file.rel], |row| row.get(0))?;
    db.execute("insert into extractions(object_id,profile_id,payload) values (?1,?2,?3) on conflict(object_id,profile_id) do nothing", params![object,profile,serde_json::to_string(extracted)?])?;
    Ok(())
}

pub fn attach_file(db: &Connection, file_id: i64, file: &SourceFile) -> Result<()> {
    let object = put(db, &file.hash, file.text.as_bytes())?;
    db.execute(
        "insert into file_objects(file_id,object_id) values (?1,?2)",
        params![file_id, object],
    )?;
    Ok(())
}

/// Read the bytes owned by the selected graph, never a later filesystem edit.
pub fn source(db: &Connection, snapshot: i64, path: &str) -> Result<Option<String>> {
    let bytes: Option<Vec<u8>> = db.query_row(
        "select c.source_bytes from files f join file_objects o on o.file_id=f.id join content_objects c on c.id=o.object_id join snapshot_manifests m on m.snapshot_id=f.repo_id where f.repo_id=?1 and f.path=?2 and m.ready=1 and m.source_complete=1",
        params![snapshot,path], |row| row.get(0),
    ).optional()?;
    bytes
        .map(|bytes| String::from_utf8(bytes).context("stored source is not UTF-8"))
        .transpose()
}
