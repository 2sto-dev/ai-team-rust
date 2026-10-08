//! The web interface: actions need the page's token and our own Host/Origin; the dashboard
//! is read-only.

mod common;

use std::time::Duration;

use ai_team::{dashboard::Sources, project::Store, web};
use common::{project, sample_company};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

async fn http(port: u16, request: String) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

fn post(port: u16, path: &str, host: &str, headers: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .replace("{port}", &port.to_string())
}

/// The page's script is checked with `node --check` when Node is installed: one syntax error
/// (a real newline inside a string) silently disabled every button on the page.
fn script_parses(page: &str) {
    let start = page.find("<script>").expect("the page has a script") + "<script>".len();
    let end = start + page[start..].find("</script>").expect("the script ends");
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("page.js");
    std::fs::write(&file, &page[start..end]).unwrap();
    match std::process::Command::new("node")
        .arg("--check")
        .arg(&file)
        .output()
    {
        Ok(output) => assert!(
            output.status.success(),
            "the page script does not parse:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(_) => eprintln!("node is not installed; the page script was not checked"),
    }
}

async fn start(writable: bool, port: u16) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let company = sample_company(dir.path());
    let store = Store::open(dir.path().join("data")).unwrap();
    let mut config = project(2);
    config.project_id = "app".to_string();
    store.add_project(&config).unwrap();
    let sources = Sources {
        data_dir: dir.path().join("data"),
        employees_dir: company.join("employees"),
        audit_dir: dir.path().join("audit"),
    };
    tokio::spawn(web::serve(sources, port, writable));
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn actions_need_the_token_and_our_own_host() {
    let port = 18_000 + (std::process::id() % 1000) as u16;
    let dir = start(true, port).await;
    let host = format!("127.0.0.1:{port}");

    let page = http(port, format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n")).await;
    let token = page
        .split("const TOKEN = \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap()
        .to_string();
    assert_eq!(token.len(), 32);
    assert!(page.contains("const WRITABLE = true"));
    script_parses(&page);

    let body = r#"{"project_id":"app","test_command":"cargo test"}"#;
    let no_token = http(port, post(port, "/api/configure", &host, "", body)).await;
    assert!(no_token.starts_with("HTTP/1.1 403"), "{no_token}");

    let token_header = format!("X-AI-Team-Token: {token}\r\n");
    let foreign_host = http(
        port,
        post(port, "/api/configure", "evil.example", &token_header, body),
    )
    .await;
    assert!(foreign_host.starts_with("HTTP/1.1 403"), "{foreign_host}");

    let foreign_origin = http(
        port,
        post(
            port,
            "/api/configure",
            &host,
            &format!("{token_header}Origin: http://evil.example\r\n"),
            body,
        ),
    )
    .await;
    assert!(
        foreign_origin.starts_with("HTTP/1.1 403"),
        "{foreign_origin}"
    );

    let ok = http(
        port,
        post(
            port,
            "/api/configure",
            &host,
            &format!("{token_header}Origin: http://{host}\r\n"),
            body,
        ),
    )
    .await;
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    let store = Store::open(dir.path().join("data")).unwrap();
    assert_eq!(
        store.project("app").unwrap().config.test_command.as_deref(),
        Some("cargo test")
    );

    // The file viewer reads only listed project files.
    for query in [
        "project=app&path=..%2FCargo.toml",
        "project=..%2F..&path=Cargo.toml",
        "project=app&path=.git%2Fconfig",
    ] {
        let refused = http(
            port,
            format!("GET /api/file?{query} HTTP/1.1\r\nHost: {host}\r\n\r\n"),
        )
        .await;
        assert!(refused.starts_with("HTTP/1.1 400"), "{query}: {refused}");
    }
    let listing = http(
        port,
        format!("GET /api/files?project=app HTTP/1.1\r\nHost: {host}\r\n\r\n"),
    )
    .await;
    assert!(
        listing.starts_with("HTTP/1.1 200") && listing.contains("\"files\""),
        "{listing}"
    );

    let snapshot = http(
        port,
        format!("GET /api/snapshot HTTP/1.1\r\nHost: {host}\r\n\r\n"),
    )
    .await;
    assert!(snapshot.contains("\"writable\":true"), "{snapshot}");

    // Nothing runs, so there is nothing to stop.
    let idle = http(port, post(port, "/api/stop", &host, &token_header, "{}")).await;
    assert!(
        idle.starts_with("HTTP/1.1 400") && idle.contains("nothing to stop"),
        "{idle}"
    );

    // A plan split is for new projects; an existing one keeps its own plan.
    let split = http(
        port,
        post(
            port,
            "/api/request",
            &host,
            &token_header,
            r#"{"prompt":"build it","project_id":"app","plan":true}"#,
        ),
    )
    .await;
    assert!(
        split.starts_with("HTTP/1.1 400") && split.contains("new project"),
        "{split}"
    );

    // Deleting needs the project id typed back; a wrong confirmation deletes nothing.
    let unconfirmed = http(
        port,
        post(
            port,
            "/api/delete_project",
            &host,
            &token_header,
            r#"{"project_id":"app","confirm":"ap"}"#,
        ),
    )
    .await;
    assert!(unconfirmed.starts_with("HTTP/1.1 400"), "{unconfirmed}");
    assert!(store.project("app").is_ok());
    let deleted = http(
        port,
        post(
            port,
            "/api/delete_project",
            &host,
            &token_header,
            r#"{"project_id":"app","confirm":"app"}"#,
        ),
    )
    .await;
    assert!(deleted.starts_with("HTTP/1.1 200"), "{deleted}");
    assert!(store.project("app").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_dashboard_is_read_only() {
    let port = 19_000 + (std::process::id() % 1000) as u16;
    let _dir = start(false, port).await;
    let host = format!("127.0.0.1:{port}");

    let page = http(port, format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n")).await;
    assert!(
        page.contains("const TOKEN = \"\""),
        "no token in read-only mode"
    );
    assert!(page.contains("const WRITABLE = false"));
    let refused = http(
        port,
        post(
            port,
            "/api/configure",
            &host,
            "X-AI-Team-Token: \r\n",
            r#"{"project_id":"app"}"#,
        ),
    )
    .await;
    assert!(refused.starts_with("HTTP/1.1 403"), "{refused}");
}
