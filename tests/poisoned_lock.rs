//! One request that panics while holding the engine lock must not take the
//! server down with it: the lock is poisoned, and every handler used to
//! `.lock().unwrap()` it.

use std::sync::Arc;
use uapp::app::App;

#[test]
fn a_panic_under_the_engine_lock_does_not_end_the_server() {
    let dir = std::env::temp_dir().join(format!("uapp-poison-{}", fastrand::u64(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let eng =
        uapp::engine::Engine::open(dir.join("p.uapp"), "dev".into(), "tester".into()).unwrap();
    let app = Arc::new(App::new(eng, "t".into(), false));
    let a = app.clone();
    let _ = std::thread::spawn(move || {
        let _held = a.engine.lock().unwrap();
        panic!("a handler bug");
    })
    .join();
    assert!(app.engine.is_poisoned());
    let info = uapp::rpc::dispatch(&app, "app.info", serde_json::json!({})).unwrap();
    assert!(info["appId"].is_string());
    uapp::rpc::dispatch(
        &app,
        "sql.exec",
        serde_json::json!({"sql": "CREATE TABLE still_works(x)"}),
    )
    .unwrap();
    drop(app);
    let _ = std::fs::remove_dir_all(&dir);
}
