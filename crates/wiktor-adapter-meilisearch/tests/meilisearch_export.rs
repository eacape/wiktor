//! Meilisearch 出口的 mock HTTP 验证（STEP10 B4，A6）。
//! Mock-HTTP verification of the Meilisearch outlet (STEP10 B4, A6).
//!
//! 用一个极简 tokio TcpListener 模拟 Meilisearch：捕获 `PUT /indexes/{uid}` 与
//! `PATCH /indexes/{uid}/documents`，断言请求体含 accepted 页的字段
//! （page_id/entity_id/title/content/content_hash/generation），且 exporter 把
//! 每个 accepted 页镜像为一个文档。qurantine 页不在 accepted_page_vectors 中，
//! 由导出方天然排除。
//! A minimal tokio TcpListener stands in for Meilisearch: it captures
//! `PUT /indexes/{uid}` and `PATCH /indexes/{uid}/documents`, asserting the body
//! carries the accepted-page fields (page_id/entity_id/title/content/
//! content_hash/generation) and that the exporter mirrors each accepted page as
//! one document. Quarantine pages are not in accepted_page_vectors, so they are
//! naturally excluded by the exporter.

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiktor_adapter_meilisearch::MeilisearchExporter;
use wiktor_core::kernel::AcceptedPageVector;

fn sample_pages() -> Vec<AcceptedPageVector> {
    vec![
        AcceptedPageVector {
            page_id: "tech-docs:concept:http".to_string(),
            entity_id: "tech-docs:concept:http".to_string(),
            title: "HTTP 协议".to_string(),
            content: "请求响应模型与状态码语义。".to_string(),
            content_hash: "hash-1".to_string(),
            generation: 1,
        },
        AcceptedPageVector {
            page_id: "tech-docs:technology:sqlite".to_string(),
            entity_id: "tech-docs:technology:sqlite".to_string(),
            title: "SQLite".to_string(),
            content: "嵌入式零配置数据库。".to_string(),
            content_hash: "hash-2".to_string(),
            generation: 1,
        },
    ]
}

/// 读一个 HTTP 请求（请求行 + 头 + body），返回 (method, path, body)。
/// 循环读直到头 + Content-Length 指定的 body 都完整（不等到 EOF，避免死锁）。
/// Reads one HTTP request (request line + headers + body) →
/// (method, path, body). Loops until the headers and the Content-Length body are
/// complete (never waits for EOF, avoiding a deadlock).
async fn read_http_request(stream: &mut tokio::net::TcpStream) -> (String, String, String) {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 1024];
    let mut content_length: Option<usize> = None;
    loop {
        let n = stream.read(&mut tmp).await.expect("read request");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        let text = String::from_utf8_lossy(&buf);
        // 头结束（空行）后解析 Content-Length，确保读够 body。
        // Once the header terminator (blank line) is present, parse
        // Content-Length so we read the whole body.
        if let Some(header_end) = text.find("\r\n\r\n") {
            if content_length.is_none() {
                for line in text[..header_end].lines() {
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().ok();
                    }
                }
            }
            let body_start = header_end + 4;
            if let Some(cl) = content_length {
                if buf.len() >= body_start + cl {
                    break;
                }
            } else {
                // 无 Content-Length（无 body 的请求，如 our ensure_index PUT）：
                // 头读完即视为请求完整，避免一直空读挂起。
                // No Content-Length (a bodyless request such as our ensure_index
                // PUT): the request is complete once the headers finish; break
                // instead of blocking forever.
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    let mut lines = text.split("\r\n");
    let req_line = lines.next().unwrap_or_default().to_string();
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string();
    (method, path, body)
}

#[tokio::test]
async fn exporter_mirrors_accepted_pages_into_mock_meilisearch() {
    // —— 启动 mock Meilisearch：接受 2 个连接（ensure_index PUT + documents
    //    PATCH），各读一请求、记录、回 202 后关闭。——
    // —— Start the mock Meilisearch: accept 2 connections (ensure_index PUT +
    //    documents PATCH), read one request each, record it, reply 202, close. ——
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap();
    let requests: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let req = read_http_request(&mut stream).await;
            eprintln!(
                "[mock] captured request: method={} path={} body_len={}",
                req.0,
                req.1,
                req.2.len()
            );
            captured.lock().unwrap().push(req);
            // Connection: close + 不设 Content-Length，让 reqwest 读到关闭即完；
            // 避免手工 Content-Length 与正文不一致导致挂起。
            // Connection: close and no Content-Length: reqwest reads until the
            // connection closes, avoiding a hand-computed Content-Length/body
            // mismatch that would hang it.
            let _ = stream
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"taskUid\":1,\"indexUid\":\"tech-docs\",\"status\":\"enqueued\"}",
                )
                .await;
            let _ = stream.shutdown().await;
        }
    });

    let base = format!("http://{addr}");
    std::env::set_var("WIKTOR_MEILISEARCH_URL", &base);
    std::env::remove_var("WIKTOR_MEILISEARCH_API_KEY");
    let exporter = MeilisearchExporter::from_env("tech-docs").expect("exporter");

    exporter.ensure_index().await.expect("ensure_index");
    let pages = sample_pages();
    let n = exporter.export_pages(&pages).await.expect("export_pages");
    assert_eq!(n, 2);

    server.await.expect("server done");
    let reqs = requests.lock().unwrap();
    assert_eq!(reqs.len(), 2, "two requests (PUT index + PATCH documents)");

    let (m1, p1, _) = &reqs[0];
    assert_eq!(m1, "PUT");
    assert!(
        p1.starts_with("/indexes/tech-docs"),
        "ensure index path: {p1}"
    );

    let (m2, p2, body2) = &reqs[1];
    assert_eq!(m2, "PATCH");
    assert!(
        p2.ends_with("/indexes/tech-docs/documents"),
        "documents path: {p2}"
    );

    let docs: Vec<serde_json::Value> = serde_json::from_str(body2).expect("body is a JSON array");
    assert_eq!(docs.len(), 2, "both accepted pages exported");
    let d0 = &docs[0];
    assert_eq!(d0["id"], "tech-docs:concept:http");
    assert_eq!(d0["entity_id"], "tech-docs:concept:http");
    assert_eq!(d0["title"], "HTTP 协议");
    assert_eq!(d0["content_hash"], "hash-1");
    assert_eq!(d0["generation"], 1);
    // every document carries the accepted-page fields (no quarantine leaks in).
    for d in &docs {
        for key in [
            "id",
            "page_id",
            "entity_id",
            "title",
            "content",
            "content_hash",
            "generation",
        ] {
            assert!(d.get(key).is_some(), "document missing field {key}");
        }
    }

    std::env::remove_var("WIKTOR_MEILISEARCH_URL");
}

#[tokio::test]
async fn empty_pages_exports_zero_without_hitting_server() {
    std::env::set_var("WIKTOR_MEILISEARCH_URL", "http://127.0.0.1:1");
    let exporter = MeilisearchExporter::from_env("empty").expect("exporter");
    let n = exporter.export_pages(&[]).await.expect("empty export");
    assert_eq!(n, 0);
    std::env::remove_var("WIKTOR_MEILISEARCH_URL");
}
