//! uapp-library — serve a directory of .uapp files (see src/library.rs).
//! Built with `--bin uapp-library --no-default-features`.
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

fn usage() -> ! {
    eprintln!(
        "uapp-library {}\n\nUsage:\n  uapp-library <dir>            serve the .uapp files in <dir>\n\nOptions:\n  --bind <addr>               interface to bind (default 127.0.0.1)\n  --port <n>                  port (default 8090)\n  --token <t>                 the secret that unlocks PUT and DELETE; else\n                              UAPP_TOKEN, else a random one is printed\n  --max-mb <n>                largest upload accepted (default 256)\n\nRoutes:\n  GET  /                      JSON listing\n  GET  /<name>.uapp           the archive (ETag; If-None-Match)\n  PUT  /<name>.uapp           publish (token; If-Match -> 409 on conflict).\n                              Stored sanitized: no chat, history or config\n  DELETE /<name>.uapp         remove (token)\n",
        env!("CARGO_PKG_VERSION")
    );
    std::process::exit(2);
}

fn main() {
    if let Err(e) = run() {
        eprintln!("uapp-library: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dir: Option<PathBuf> = None;
    let mut bind = "127.0.0.1".to_string();
    let mut port: u16 = 8090;
    let mut max_mb: usize = 256;
    let mut token: Option<String> = std::env::var("UAPP_TOKEN").ok().filter(|t| !t.is_empty());
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--bind" => bind = it.next().cloned().unwrap_or_else(|| usage()),
            "--port" => {
                port = it.next().and_then(|p| p.parse().ok()).unwrap_or_else(|| {
                    eprintln!("uapp-library: --port needs a number");
                    usage()
                })
            }
            "--max-mb" => {
                max_mb = it.next().and_then(|p| p.parse().ok()).unwrap_or_else(|| {
                    eprintln!("uapp-library: --max-mb needs a number");
                    usage()
                })
            }
            "--token" => token = Some(it.next().cloned().unwrap_or_else(|| usage())),
            "-h" | "--help" => usage(),
            "-V" | "--version" => {
                println!("uapp-library {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other if other.starts_with('-') => {
                eprintln!("uapp-library: unknown option {other}");
                usage()
            }
            _ => {
                if dir.is_some() {
                    usage();
                }
                dir = Some(PathBuf::from(a));
            }
        }
    }
    let dir = dir.unwrap_or_else(|| usage());
    let dir = if dir.is_absolute() { dir } else { std::env::current_dir()?.join(dir) };
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let generated = token.is_none();
    let token = token.unwrap_or_else(|| (0..32).map(|_| fastrand::alphanumeric()).collect());
    let lib = Arc::new(uapp::library::Library { dir: dir.clone(), token: token.clone(), max_bytes: max_mb * 1024 * 1024 });

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let listener = tokio::net::TcpListener::bind((bind.as_str(), port)).await?;
        let addr = listener.local_addr()?;
        eprintln!("uapp-library: serving {} at http://{}/", dir.display(), addr);
        if generated {
            eprintln!("uapp-library: token (pass --token or set UAPP_TOKEN to fix it): {token}");
        }
        if std::fs::metadata(&dir).map(|m| m.permissions().readonly()).unwrap_or(true) {
            eprintln!("uapp-library: {} is not writable — PUT will fail", dir.display());
        }
        axum::serve(listener, uapp::library::router(lib))
            .with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; })
            .await?;
        Ok::<_, anyhow::Error>(())
    })
}
