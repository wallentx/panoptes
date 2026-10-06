//! Small self-contained viewer. No Node runtime and no committed generated
//! assets: export one HTML file or serve it on loopback.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::path::Path;

use crate::index;

/// Pin the checkout attachment through every map query, then release the view
/// before the caller writes the output or starts a long-running HTTP server.
pub fn render_current(db: &rusqlite::Connection, root: &Path, title: &str) -> Result<String> {
    render_current_with(db, root, title, || {})
}

fn render_current_with(
    db: &rusqlite::Connection,
    root: &Path,
    title: &str,
    after_select: impl FnOnce(),
) -> Result<String> {
    let view = db.unchecked_transaction()?;
    let id =
        index::repo_id_of(&view, root)?.context("checkout index was reset before visualization")?;
    after_select();
    let html = render(&view, id, title)?;
    view.commit()?;
    Ok(html)
}

pub fn render(db: &rusqlite::Connection, repo_id: i64, title: &str) -> Result<String> {
    let map = index::repo_map(db, repo_id, 100)?;
    let data = serde_json::to_string(&map)?.replace("</", "<\\/");
    Ok(format!(
        r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Panoptes - {title}</title>
<style>
body{{font:15px system-ui,sans-serif;max-width:1100px;margin:2rem auto;padding:0 1rem;background:#111;color:#eee}}
h1,h2{{color:#9ee37d}} .card{{border:1px solid #444;border-radius:8px;padding:1rem;margin:.7rem 0}}
.muted{{color:#aaa}} code{{color:#8bd5ff}} ul{{line-height:1.6}}
</style>
<h1>Panoptes - {title}</h1><div id="app">Loading...</div>
<script>const data={data};const e=s=>String(s).replace(/[&<>"']/g,c=>({{'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}}[c]));
document.querySelector('#app').innerHTML=`<p class=muted>${{data.files}} files - ${{data.symbols}} symbols - ${{data.edges}} edges</p><h2>Directories</h2>${{data.dirs.map(d=>`<div class=card><b>${{e(d.dir)}}</b> - ${{d.files}} files - ${{d.symbols}} symbols${{d.hubs.length?`<ul>${{d.hubs.map(h=>`<li><code>${{e(h.name)}}</code> (${{h.in_degree}} callers) - ${{e(h.path)}}:${{h.start_line}}</li>`).join('')}}</ul>`:''}}</div>`).join('')}}<h2>Hotspots</h2><ol>${{data.hotspots.map(h=>`<li><code>${{e(h.name)}}</code> (${{h.in_degree}} callers) - ${{e(h.path)}}:${{h.start_line}}</li>`).join('')}}</ol>`;</script>
"#
    ))
}

pub fn write(path: &Path, html: &str, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!("{} exists; pass --force to replace it", path.display());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, html)?;
    Ok(())
}

pub fn serve(bind: &str, html: String, allow_remote: bool) -> Result<()> {
    let listener = TcpListener::bind(bind).with_context(|| format!("bind {bind}"))?;
    let address = listener.local_addr()?;
    if !allow_remote && !is_loopback(address) {
        bail!("refusing non-loopback bind {address}; pass --allow-remote explicitly");
    }
    println!("panoptes viz: http://{address}/");
    println!("press Ctrl-C to stop");
    for stream in listener.incoming() {
        let mut stream = stream?;
        let mut request = [0_u8; 8192];
        let size = stream.read(&mut request)?;
        let request = String::from_utf8_lossy(&request[..size]);
        let ok = request.starts_with("GET / ") || request.starts_with("GET /index.html ");
        let (status, body, content_type) = if ok {
            ("200 OK", html.as_str(), "text/html; charset=utf-8")
        } else {
            ("404 Not Found", "not found\n", "text/plain; charset=utf-8")
        };
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self' 'unsafe-inline'\r\n\r\n{body}",
            body.len()
        )?;
    }
    Ok(())
}

fn is_loopback(address: SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_pins_the_selected_snapshot_during_rebuild_and_reclamation() {
        let root =
            std::env::temp_dir().join(format!("panoptes-viz-snapshot-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.rs"), "pub fn original() {}\n").unwrap();
        let store = root.join("store.db");
        let mut writer = crate::db::open(&store).unwrap();
        index::build(&mut writer, &root).unwrap();
        let reader = crate::db::open(&store).unwrap();
        let old = index::repo_id_of(&reader, &root).unwrap();
        let html = render_current_with(&reader, &root, "fixture", || {
            std::fs::write(root.join("a.rs"), "pub fn replacement() {}\n").unwrap();
            std::fs::write(root.join("b.rs"), "pub fn second() {}\n").unwrap();
            index::build(&mut writer, &root).unwrap();
            assert_ne!(old, index::repo_id_of(&writer, &root).unwrap());
        })
        .unwrap();
        assert!(html.contains("\"files\":1"), "{html}");
        assert!(reader.is_autocommit());
        assert!(
            render_current(&reader, &root, "fixture")
                .unwrap()
                .contains("\"files\":2")
        );
        drop((reader, writer));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn only_loopback_is_accepted_by_default() {
        assert!(is_loopback("127.0.0.1:8080".parse().unwrap()));
        assert!(is_loopback("[::1]:8080".parse().unwrap()));
        assert!(!is_loopback("0.0.0.0:8080".parse().unwrap()));
    }
}
