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
