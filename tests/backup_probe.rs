// Probe: which SQLite backup paths work with SQLCipher in this build?
use rusqlite::Connection;
use std::time::Duration;

fn open_keyed(path: &str, pw: &str) -> Connection {
    let c = Connection::open(path).unwrap();
    c.pragma_update(None, "key", format!("'{}'", pw)).unwrap();
    c
}

/// Run a backup to completion; the Backup (and its borrow) is dropped on return.
fn do_backup<'a>(src: &Connection, dst: &'a mut Connection) -> Result<(), String> {
    let bk = rusqlite::backup::Backup::new(src, dst).map_err(|e| e.to_string())?;
    bk.run_to_completion(512, Duration::from_millis(0), None)
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[test]
fn probe_backup_modes() {
    let dir = std::env::temp_dir().join("uapp-probe");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("a.db");
    let pw = "pw";

    // encrypted source with data
    let src = open_keyed(f.to_str().unwrap(), pw);
    src.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (42);").unwrap();

    // 1) encrypted src -> plain :memory:
    let mut mem = Connection::open_in_memory().unwrap();
    match do_backup(&src, &mut mem) {
        Ok(()) => println!("PROBE enc->plain-mem: ok"),
        Err(e) => println!("PROBE enc->plain-mem: ERR {e}"),
    }

    // 2) encrypted src -> keyed :memory: (same passphrase)
    let mut mem2 = Connection::open_in_memory().unwrap();
    match mem2.pragma_update(None, "key", format!("'{}'", pw)) {
        Ok(()) => match do_backup(&src, &mut mem2) {
            Ok(()) => {
                let v: i64 = mem2.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap_or(-1);
                println!("PROBE enc->keyed-mem: ok, data={v}");
            }
            Err(e) => println!("PROBE enc->keyed-mem: BACKUP ERR {e}"),
        },
        Err(e) => println!("PROBE enc->keyed-mem: PRAGMA key on :memory: ERR {e}"),
    }

    // 3) plain src -> encrypted dst file (encrypt-in-place path)
    let plain = dir.join("p.db");
    let psrc = Connection::open(&plain).unwrap();
    psrc.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (7);").unwrap();
    let mut dst = open_keyed(dir.join("e2.db").to_str().unwrap(), pw);
    match do_backup(&psrc, &mut dst) {
        Ok(()) => {
            let v: i64 = dst.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap_or(-1);
            println!("PROBE plain->enc-file: ok, data={v}");
        }
        Err(e) => println!("PROBE plain->enc-file: ERR {e}"),
    }

    // 4) encrypted src -> encrypted dst file, same passphrase (compaction path)
    let mut dst2 = open_keyed(dir.join("e3.db").to_str().unwrap(), pw);
    match do_backup(&src, &mut dst2) {
        Ok(()) => {
            let v: i64 = dst2.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap_or(-1);
            println!("PROBE enc->enc-file: ok, data={v}");
        }
        Err(e) => println!("PROBE enc->enc-file: ERR {e}"),
    }
}
