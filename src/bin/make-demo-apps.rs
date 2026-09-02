//! Build the launcher.uapp for the browser demo.
//! Usage: cargo run --bin make-demo-apps -- <launcher-out-dir>
//!
//! Reads from templates/ at runtime:
//! - templates/launcher.html - main launcher UI
//! - templates/samples.json - catalog of demo apps

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use rusqlite::{params, Connection, OpenFlags};

const STABLE_MS: i64 = 1_755_000_000_000;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let outdir = args.get(1).map(|s| s.as_str()).unwrap_or("dist-web");

    fs::create_dir_all(outdir)?;

    let templates_dir = Path::new("templates");

    // Read launcher HTML
    let launcher_html = fs::read_to_string(templates_dir.join("launcher.html"))
        .expect("Failed to read templates/launcher.html");

    // Read samples catalog
    let samples_content = fs::read_to_string(templates_dir.join("samples.json"))
        .expect("Failed to read templates/samples.json");
    let samples: Vec<Sample> = serde_json::from_str(&samples_content)
        .expect("Failed to parse templates/samples.json");

    build_launcher(outdir, &samples, &launcher_html)?;

    Ok(())
}

#[derive(serde::Deserialize, Clone)]
struct Sample {
    name: String,
    file: String,
    emoji: String,
    description: String,
    website: Option<String>,
    /// Which shelf of the launcher tray it sits on: "app" (the default) or "game".
    #[serde(default)]
    kind: Option<String>,
}

fn build_launcher(outdir: &str, samples: &[Sample], html: &str) -> std::io::Result<()> {
    let path = PathBuf::from(outdir).join("launcher.uapp");
    let _ = fs::remove_file(&path);

    let mut files = HashMap::new();
    files.insert("app/index.html".to_string(), html.as_bytes().to_vec());

    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
    ).unwrap();

    conn.execute_batch(
        "CREATE TABLE sqlar(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB);
         CREATE TABLE uapp_meta(key TEXT PRIMARY KEY, value TEXT);
         CREATE TABLE uapp_config(key TEXT PRIMARY KEY, value TEXT);
         CREATE TABLE uapp_chat(
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           mid TEXT UNIQUE, ts INTEGER, ctr INTEGER, device TEXT, user TEXT,
           role TEXT, content TEXT, session TEXT);
         CREATE TABLE uapp_sessions(
           id TEXT PRIMARY KEY, title TEXT, created INTEGER,
           kind TEXT DEFAULT 'chat', parent TEXT, archived INTEGER DEFAULT 0);
         CREATE TABLE uapp_file_history(
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           name TEXT, ts INTEGER, device TEXT, user TEXT, action TEXT, data BLOB, sz INT);
         CREATE TABLE samples(
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           name TEXT NOT NULL, url TEXT NOT NULL, emoji TEXT, description TEXT, website TEXT,
           size INTEGER, kind TEXT NOT NULL DEFAULT 'app');"
    ).unwrap();

    let app_id = random_id();
    conn.execute("INSERT INTO uapp_meta(key,value) VALUES(?,?)", params!["app_id", &app_id]).unwrap();
    conn.execute("INSERT INTO uapp_meta(key,value) VALUES(?,?)", params!["name", "Apps"]).unwrap();
    conn.execute("INSERT INTO uapp_meta(key,value) VALUES(?,?)", params!["format_version", "1"]).unwrap();
    conn.execute("INSERT INTO uapp_meta(key,value) VALUES(?,?)", params!["created", &STABLE_MS.to_string()]).unwrap();

    conn.execute("INSERT INTO sqlar(name,mode,mtime,sz,data) VALUES(?,?,?,?,?)", params![
        "app/index.html", 420i32, STABLE_MS / 1000, html.len() as i64, html.as_bytes()
    ]).unwrap();

    // Where the launcher fetches samples from: next to itself by default
    // (the static demo's examples/), or a uapp-library shelf — e.g.
    // UAPP_SAMPLES_BASE=/uapp/apps/ when the launcher is served from one.
    let samples_base = env::var("UAPP_SAMPLES_BASE").unwrap_or_else(|_| "examples/".into());
    let mut stmt = conn
        .prepare("INSERT INTO samples(name,url,emoji,description,website,size,kind) VALUES(?,?,?,?,?,?,?)")
        .unwrap();
    for sample in samples {
        // The launcher shows the byte size on each chip: a .uapp reads as a file
        // you can pick up, so it should carry a file's weight. 0 if it isn't built
        // yet (the catalog can list a sample before examples/ has it).
        let size = fs::metadata(Path::new("examples").join(&sample.file))
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        stmt.execute(params![
            &sample.name,
            &format!("{}{}", samples_base, sample.file),
            &sample.emoji,
            &sample.description,
            &sample.website,
            size,
            sample.kind.as_deref().unwrap_or("app"),
        ]).unwrap();
    }

    println!("wrote {} ({} bytes)", path.display(), fs::metadata(&path)?.len());
    Ok(())
}

fn random_id() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = fastrand::Rng::new();
    (0..16).map(|_| {
        let idx = rng.usize(..CHARSET.len());
        CHARSET[idx] as char
    }).collect()
}

use std::collections::HashMap;
