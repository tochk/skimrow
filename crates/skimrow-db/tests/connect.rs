//! Integration tests against a real PostgreSQL server.
//!
//! Skipped unless `SKIMROW_TEST_HOST`/`SKIMROW_TEST_USER`/`SKIMROW_TEST_DB` are set (and
//! optionally `SKIMROW_TEST_PASSWORD`/`SKIMROW_TEST_PORT`). No docker here, so these run
//! against a native server. See the dev-env memory.

use skimrow_db::{
    CellValue, ConnectPhase, ConnectionConfig, DbHandle, QueryEvent, Session, SslMode,
};

fn test_config() -> Option<ConnectionConfig> {
    let host = std::env::var("SKIMROW_TEST_HOST").ok()?;
    let user = std::env::var("SKIMROW_TEST_USER").ok()?;
    let db = std::env::var("SKIMROW_TEST_DB").ok()?;
    let mut cfg = ConnectionConfig::new("test", host, db, user);
    cfg.password = std::env::var("SKIMROW_TEST_PASSWORD").ok();
    if let Ok(p) = std::env::var("SKIMROW_TEST_PORT") {
        cfg.port = p.parse().expect("SKIMROW_TEST_PORT");
    }
    cfg.ssl_mode = SslMode::Disable;
    Some(cfg)
}

/// Drain a query stream into (columns, rows, last affected count).
async fn collect(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<QueryEvent>,
) -> (Vec<String>, Vec<Vec<CellValue>>, u64) {
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    let mut affected = 0;
    while let Some(ev) = rx.recv().await {
        match ev {
            QueryEvent::Columns(c) => columns = c.iter().map(|m| m.name.clone()).collect(),
            QueryEvent::Rows(mut r) => rows.append(&mut r),
            QueryEvent::Done { affected: a, .. } => affected = a,
            QueryEvent::Failed(e) => panic!("query failed: {e}"),
            QueryEvent::Transaction(_) => {}
        }
    }
    (columns, rows, affected)
}

#[tokio::test]
async fn select_values_and_nulls() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: SKIMROW_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");

    let mut rx = db.query(
        conn,
        "SELECT 1 AS one, 'hi'::text AS greeting, NULL::int AS nope",
    );
    let (cols, rows, _) = collect(&mut rx).await;

    assert_eq!(cols, ["one", "greeting", "nope"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CellValue::Text("1".into()));
    assert_eq!(rows[0][1], CellValue::Text("hi".into()));
    assert_eq!(rows[0][2], CellValue::Null);
}

#[tokio::test]
async fn dml_roundtrip_in_one_transaction() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: SKIMROW_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");
    // Pooled: an open transaction keeps the session on one connection, so
    // the temp table is there for every run until it ends.
    let conn = Session::new(conn, 1);

    let mut rx = db.query(conn, "BEGIN");
    let (_, _, _) = collect(&mut rx).await;
    let mut rx = db.query(conn, "CREATE TEMP TABLE t (id int primary key, name text)");
    let (_, _, _) = collect(&mut rx).await;

    let mut rx = db.query(conn, "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,NULL)");
    let (_, _, affected) = collect(&mut rx).await;
    assert_eq!(affected, 3, "INSERT should report 3 rows");

    let mut rx = db.query(conn, "UPDATE t SET name='z' WHERE id=2");
    let (_, _, affected) = collect(&mut rx).await;
    assert_eq!(affected, 1, "UPDATE should report 1 row");

    let mut rx = db.query(conn, "SELECT id, name FROM t ORDER BY id");
    let (cols, rows, _) = collect(&mut rx).await;
    assert_eq!(cols, ["id", "name"]);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1][1], CellValue::Text("z".into()));
    assert_eq!(rows[2][1], CellValue::Null);

    let mut rx = db.query(conn, "ROLLBACK");
    let (_, _, _) = collect(&mut rx).await;
}

#[tokio::test]
async fn reconnects_after_connection_lost() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: SKIMROW_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");

    // Kill this session's backend server-side: the driver exits and the
    // client reports closed — the same state as a server restart / idle drop.
    let mut rx = db.query(conn, "SELECT pg_terminate_backend(pg_backend_pid())");
    while rx.recv().await.is_some() {} // drain; ends in Failed, expected

    // Give the driver task a moment to observe the FIN and mark the client closed.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // The next query on the SAME ConnId must transparently reconnect.
    let mut rx = db.query(conn, "SELECT 41 + 1 AS answer");
    let (cols, rows, _) = collect(&mut rx).await;
    assert_eq!(cols, ["answer"]);
    assert_eq!(rows[0][0], CellValue::Text("42".into()));
}

#[tokio::test]
async fn error_surfaces_with_sqlstate() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: SKIMROW_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");

    let mut rx = db.query(conn, "SELECT * FROM definitely_not_a_table");
    let mut failed = None;
    while let Some(ev) = rx.recv().await {
        if let QueryEvent::Failed(e) = ev {
            failed = Some(e);
        }
    }
    match failed.expect("should fail") {
        skimrow_db::DbError::Postgres { code, .. } => {
            assert_eq!(code.as_deref(), Some("42P01")); // undefined_table
        }
        other => panic!("expected Postgres error, got {other:?}"),
    }
}

/// `test` reports the server version, and an error for a database that
/// isn't there.
#[tokio::test]
async fn test_reports_version() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: SKIMROW_TEST_* not set");
        return;
    };
    let db = DbHandle::spawn();
    let version = db.test(cfg.clone()).await.expect("test");
    assert!(version.starts_with("PostgreSQL "), "version: {version}");
    let mut bad = cfg;
    bad.dbname = "skimrow_no_such_database".into();
    assert!(db.test(bad).await.is_err());
}

/// A server that accepts connections and never says a word: what a dead
/// link (or a black-holing firewall past the TCP handshake) looks like.
/// Returns its port and a receiver told each time a client hangs up.
async fn silent_server() -> (u16, tokio::sync::mpsc::UnboundedReceiver<()>) {
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let (hung_up, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let hung_up = hung_up.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                // Read (and ignore) whatever the client sends until it closes.
                while let Ok(n) = socket.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                }
                let _ = hung_up.send(());
            });
        }
    });
    (port, rx)
}

fn silent_config(port: u16, timeout: std::time::Duration) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new("silent", "127.0.0.1", "db", "user");
    cfg.port = port;
    cfg.ssl_mode = SslMode::Disable;
    cfg.connect_timeout = Some(timeout);
    cfg
}

/// A server that never answers fails the connect after `connect_timeout`
/// (tokio-postgres' own timeout covers only the TCP connect), reporting
/// that it was reaching the server.
#[tokio::test]
async fn connect_times_out_on_a_silent_server() {
    let (port, _) = silent_server().await;
    let db = DbHandle::spawn();
    let limit = std::time::Duration::from_millis(300);
    let started = std::time::Instant::now();
    let (mut phases, opening) = db.open_watched(silent_config(port, limit));
    match opening.await {
        Err(skimrow_db::DbError::Timeout(d)) => assert_eq!(d, limit),
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(phases.recv().await, Some(ConnectPhase::Server));
    // A test of the same config gives up the same way.
    match db.test(silent_config(port, limit)).await {
        Err(skimrow_db::DbError::Timeout(_)) => {}
        other => panic!("expected a timeout, got {other:?}"),
    }
}

/// Dropping a connect's future abandons it: the worker stops dialing and
/// hangs up on the server.
#[tokio::test]
async fn abandoned_connect_hangs_up() {
    let (port, mut hung_up) = silent_server().await;
    let db = DbHandle::spawn();
    let (_, opening) = db.open_watched(silent_config(port, std::time::Duration::from_secs(60)));
    let opening = tokio::spawn(opening);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    opening.abort();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), hung_up.recv()).await;
    assert!(matches!(closed, Ok(Some(()))), "the dial wasn't dropped");
}

/// A connect stuck on a slow server doesn't hold up another connection's
/// work: connects run off the worker loop.
#[tokio::test]
async fn slow_connect_leaves_other_connections_alone() {
    let Some(cfg) = test_config() else {
        eprintln!("skipping: SKIMROW_TEST_* not set");
        return;
    };
    let (port, _) = silent_server().await;
    let db = DbHandle::spawn();
    let conn = db.connect(cfg).await.expect("connect");
    let stuck = db.clone();
    let _slow = tokio::spawn(async move {
        stuck
            .open(silent_config(port, std::time::Duration::from_secs(60)))
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut rx = db.query(conn, "SELECT 1");
    let ran = tokio::time::timeout(std::time::Duration::from_secs(5), collect(&mut rx)).await;
    assert!(ran.is_ok(), "the query waited for the stuck connect");
}
