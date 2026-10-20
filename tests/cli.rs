//! The `uapp` command line: exit codes, typo handling, and the file commands
//! (new, info, export, encrypt/decrypt/passwd) end to end. Each run gets its
//! own HOME so nothing touches the real per-user state.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Sandbox {
        let dir = std::env::temp_dir().join(format!("uapp-cli-{name}-{}", fastrand::u32(..)));
        std::fs::create_dir_all(dir.join("home")).unwrap();
        Sandbox { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_uapp"));
        c.args(args)
            .current_dir(&self.dir)
            .env("HOME", self.dir.join("home"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("UAPP_PASSWORD")
            .env_remove("UAPP_NEW_PASSWORD")
            .env_remove("UAPP_LOG")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    /// Run with `input` on stdin.
    fn run_stdin(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .cmd(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}
fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
fn is_sqlite(p: &Path) -> bool {
    std::fs::read(p)
        .map(|b| b.starts_with(b"SQLite format 3"))
        .unwrap_or(false)
}

#[test]
fn help_version_and_usage_errors() {
    let s = Sandbox::new("help");
    let o = s.run(&["--help"]);
    assert_eq!(code(&o), 0);
    assert!(out(&o).contains("Commands:"));
    for sub in [
        "open",
        "new",
        "update",
        "export",
        "info",
        "encrypt",
        "decrypt",
        "passwd",
        "serve",
        "library",
        "install",
        "uninstall",
        "completions",
    ] {
        let o = s.run(&[sub, "--help"]);
        assert_eq!(code(&o), 0, "{sub} --help");
        assert!(out(&o).contains("Usage: uapp"), "{sub} --help");
    }
    let o = s.run(&["--version"]);
    assert_eq!(code(&o), 0);
    assert_eq!(
        out(&o).trim(),
        format!("uapp {}", env!("CARGO_PKG_VERSION"))
    );
    // An unknown option or a missing argument is a usage error.
    assert_eq!(code(&s.run(&["info", "--nope"])), 2);
    assert_eq!(code(&s.run(&["export", "x.uapp"])), 2);
    // Options of a bare `uapp` don't go before a command.
    assert_eq!(code(&s.run(&["--headless", "info", "x.uapp"])), 2);
    // serve flags that only apply in the other mode
    assert_eq!(code(&s.run(&["serve", "--publish-data"])), 2);
    assert_eq!(code(&s.run(&["serve"])), 2);
}

#[test]
fn a_typo_is_an_error_not_a_new_file() {
    let s = Sandbox::new("typo");
    let o = s.run(&["opne"]);
    assert_eq!(code(&o), 2);
    assert!(err(&o).contains("unrecognized subcommand"), "{}", err(&o));
    assert!(
        err(&o).contains("open"),
        "suggests the right one: {}",
        err(&o)
    );
    assert!(!s.path("opne").exists());
    assert_eq!(std::fs::read_dir(&s.dir).unwrap().count(), 1, "only home/");
}

#[test]
fn file_commands_check_the_file_first() {
    let s = Sandbox::new("missing");
    for sub in ["encrypt", "decrypt", "passwd", "info"] {
        let o = s.run(&[sub, "missing.uapp"]);
        assert_eq!(code(&o), 1, "{sub}");
        assert!(err(&o).contains("doesn't exist"), "{sub}: {}", err(&o));
    }
    let o = s.run(&["export", "missing.uapp", "-o", "out.uapp"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("doesn't exist"));
    let o = s.run(&["update", "missing.uapp", "tpl.uapp"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("doesn't exist"));
    assert!(!s.path("missing.uapp").exists());
}

#[test]
fn new_info_export() {
    let s = Sandbox::new("info");
    let o = s.run(&["new", "a.uapp"]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let o = s.run(&["info", "a.uapp", "--json"]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["name"], "a");
    assert_eq!(v["encrypted"], false);
    assert_eq!(v["format_version"], 1);
    assert!(v["app_id"].as_str().is_some_and(|id| !id.is_empty()));
    assert!(v["files"]["app"].as_i64().unwrap() >= 1);
    // A table with rows shows up with its count.
    {
        let c = rusqlite::Connection::open(s.path("a.uapp")).unwrap();
        c.execute_batch("CREATE TABLE notes(t); INSERT INTO notes VALUES ('x'), ('y');")
            .unwrap();
    }
    let v: serde_json::Value =
        serde_json::from_str(&out(&s.run(&["info", "a.uapp", "--json"]))).unwrap();
    assert_eq!(
        v["tables"],
        serde_json::json!([{"name": "notes", "rows": 2}])
    );
    let text = out(&s.run(&["info", "a.uapp"]));
    assert!(text.contains("notes") && text.contains("2 rows"), "{text}");

    let o = s.run(&["export", "a.uapp", "-o", "copy.uapp"]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let v: serde_json::Value =
        serde_json::from_str(&out(&s.run(&["info", "copy.uapp", "--json"]))).unwrap();
    assert_eq!(v["tables"][0]["rows"], 2, "a full copy keeps the rows");
    // No silent overwrite.
    let o = s.run(&["export", "a.uapp", "-o", "copy.uapp"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("already exists"));

    let o = s.run(&["export", "a.uapp", "-o", "tpl.uapp", "--template"]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let v: serde_json::Value =
        serde_json::from_str(&out(&s.run(&["info", "tpl.uapp", "--json"]))).unwrap();
    assert_eq!(v["tables"][0]["rows"], 0, "a template has empty tables");
}

#[test]
fn encrypt_round_trip_with_stdin_passwords() {
    let s = Sandbox::new("enc");
    let f = s.path("e.uapp");
    assert_eq!(code(&s.run(&["new", "e.uapp"])), 0);

    let o = s.run_stdin(&["encrypt", "e.uapp", "--password-stdin"], "pw one\n");
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(!is_sqlite(&f));

    // A wrong password fails in plain words, without SQLCipher's own log lines.
    let o = s.run_stdin(&["info", "e.uapp", "--password-stdin"], "nope\n");
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("wrong master password"), "{}", err(&o));
    assert!(!err(&o).contains("ERROR CORE"), "{}", err(&o));

    // No password and no terminal: a clear error, not a hang.
    let o = s.run(&["info", "e.uapp"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("--password-stdin"), "{}", err(&o));

    let o = s.run_stdin(
        &["info", "e.uapp", "--json", "--password-stdin"],
        "pw one\n",
    );
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["encrypted"], true);
    assert_eq!(v["format_version"], 1);

    // passwd: the second stdin line is the new password.
    let o = s.run_stdin(
        &["passwd", "e.uapp", "--password-stdin"],
        "pw one\npw two\n",
    );
    assert_eq!(code(&o), 0, "{}", err(&o));
    std::fs::write(s.path("pw.txt"), "pw two\n").unwrap();
    let o = s.run(&["info", "e.uapp", "--password-file", "pw.txt"]);
    assert_eq!(code(&o), 0, "{}", err(&o));

    // passwd: UAPP_PASSWORD (old) + UAPP_NEW_PASSWORD (new).
    let o = s
        .cmd(&["passwd", "e.uapp"])
        .env("UAPP_PASSWORD", "pw two")
        .env("UAPP_NEW_PASSWORD", "pw three")
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));

    // An encrypted export stays encrypted with the same password.
    let o = s
        .cmd(&["export", "e.uapp", "-o", "e2.uapp"])
        .env("UAPP_PASSWORD", "pw three")
        .output()
        .unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(!is_sqlite(&s.path("e2.uapp")));

    let o = s.run_stdin(&["decrypt", "e.uapp", "--password-stdin"], "pw three\n");
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(is_sqlite(&f));
    let v: serde_json::Value =
        serde_json::from_str(&out(&s.run(&["info", "e.uapp", "--json"]))).unwrap();
    assert_eq!(v["encrypted"], false);
    assert_eq!(v["format_version"], 1, "the header survives the round trip");
    let c = rusqlite::Connection::open(&f).unwrap();
    let app_id: i32 = c
        .query_row("PRAGMA application_id", [], |r| r.get(0))
        .unwrap();
    assert_eq!(app_id, uapp::store::APPLICATION_ID);
}

#[test]
fn completions() {
    let s = Sandbox::new("comp");
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let o = s.run(&["completions", shell]);
        assert_eq!(code(&o), 0, "{shell}");
        assert!(out(&o).contains("uapp"), "{shell}");
    }
    assert_eq!(code(&s.run(&["completions", "nosh"])), 2);
}
