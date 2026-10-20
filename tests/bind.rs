//! Default bind is dual-stack: /healthz answers on both 127.0.0.1 and ::1.
mod common;
use common::*;

#[tokio::test]
async fn healthz_on_ipv4_and_ipv6_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, base) = spawn_bin(dir.path()).await; // waits on 127.0.0.1
    let v4 = reqwest::get(format!("{base}/healthz"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(v4, "ok");
    if std::net::TcpListener::bind("[::1]:0").is_ok() {
        let v6 = base.replace("127.0.0.1", "[::1]");
        let body = reqwest::get(format!("{v6}/healthz"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(body, "ok");
    } else {
        eprintln!("no IPv6 on this host; skipping ::1 check");
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `openfault healthcheck` (the Dockerfile HEALTHCHECK): 0 against a live instance, 1 on a closed port.
#[tokio::test]
async fn healthcheck_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, base) = spawn_bin(dir.path()).await;
    let port = base.rsplit(':').next().unwrap().to_string();
    let run = |port: String| {
        std::process::Command::new(env!("CARGO_BIN_EXE_openfault"))
            .arg("healthcheck")
            .env("OPENFAULT_PORT", port)
            .output()
            .unwrap()
    };
    let ok = run(port);
    assert_eq!(ok.status.code(), Some(0));
    assert!(ok.stdout.is_empty() && ok.stderr.is_empty());
    let _ = child.kill();
    let _ = child.wait();
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert_eq!(run(closed.to_string()).status.code(), Some(1));
}
