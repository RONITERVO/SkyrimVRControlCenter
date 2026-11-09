use crate::{archive::Archive, blocking, config::*, engine::Engine, lyrics, song::*};
use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, Limited, StreamBody};
use hyper::{
    body::{Frame, Incoming},
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{convert::Infallible, path::PathBuf, sync::Arc, time::Duration};
type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, BoxError>;
fn response(bytes: impl Into<Bytes>, kind: &str, status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("Content-Type", kind)
        .body(
            Full::new(bytes.into())
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap()
}
fn json_response(v: &Value, status: StatusCode) -> Response<Body> {
    response(serde_json::to_vec(v).unwrap(), "application/json", status)
}
pub fn chat_response(answer: &str, stream: bool, model: &str) -> (String, &'static str) {
    let created = crate::engine::now() as u64;
    let common = |object: &str, choices: Value| json!({"id":"local-native-bard","created":created,"model":model,"object":object,"choices":choices});
    if stream {
        let first = common(
            "chat.completion.chunk",
            json!([{"index":0,"delta":{"content":answer},"finish_reason":null}]),
        );
        let last = common(
            "chat.completion.chunk",
            json!([{"index":0,"delta":{},"finish_reason":"stop"}]),
        );
        (
            format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        )
    } else {
        (common("chat.completion",json!([{"index":0,"message":{"role":"assistant","content":answer},"finish_reason":"stop"}])).to_string(),"application/json")
    }
}

async fn relay(
    raw: Bytes,
    headers: hyper::HeaderMap,
    client: reqwest::Client,
    key: String,
    endpoint: &str,
) -> Result<Response<Body>> {
    let mut request = client
        .post(endpoint)
        .bearer_auth(key)
        .header("Content-Type", "application/json")
        .body(raw)
        .timeout(Duration::from_secs(300));
    for name in ["HTTP-Referer", "X-Title", "X-OpenRouter-Title"] {
        if let Some(value) = headers.get(name) {
            request = request.header(name, value);
        }
    }
    let upstream = tokio::time::timeout(Duration::from_secs(30), request.send())
        .await
        .context("Relay response header timeout")?
        .context("Upstream relay unavailable")?;
    let status = upstream.status();
    let mut builder = Response::builder().status(status);
    for name in [
        "content-type",
        "content-encoding",
        "cache-control",
        "x-request-id",
        "retry-after",
    ] {
        if let Some(value) = upstream.headers().get(name) {
            builder = builder.header(name, value);
        }
    }
    let stream = upstream.bytes_stream().map(|result| {
        result
            .map(Frame::data)
            .map_err(|e| -> BoxError { Box::new(e) })
    });
    Ok(builder.body(StreamBody::new(stream).boxed_unsync())?)
}
async fn relay_key(root: PathBuf) -> Result<String> {
    // No credential cache: a pool can survive a key/settings change safely.
    blocking::run(move || openrouter_key(&read_settings(&root)?)).await
}
async fn handle(
    req: Request<Incoming>,
    engine: Arc<Engine>,
    archive: Arc<Archive>,
) -> Result<Response<Body>> {
    let path = req.uri().path().to_owned();
    if req.method() == Method::HEAD {
        return Ok(response(
            "",
            "application/json",
            if path == "/" || path == "/health" {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            },
        ));
    }
    if req.method() == Method::GET && path == "/health" {
        let mut status = blocking::run(move || engine.status()).await?;
        status["song_downloads"] = archive.status();
        status["automatic_secondary_capture"] = json!(status["song_downloads"]["auto"] == true);
        status["automatic_secondary_import"] = json!(false);
        return Ok(json_response(&status, StatusCode::OK));
    }
    if req.method() != Method::POST {
        return Ok(json_response(
            &json!({"error":"Not found"}),
            StatusCode::NOT_FOUND,
        ));
    }
    let root = engine.root.clone();
    let expected = blocking::run(move || Ok(format!("Bearer {}", token(&root)?))).await?;
    if req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        != Some(expected.as_str())
    {
        return Ok(json_response(
            &json!({"error":"Unauthorized"}),
            StatusCode::UNAUTHORIZED,
        ));
    }
    let (headers, body) = {
        let (parts, body) = req.into_parts();
        (parts.headers, body)
    };
    let raw = Limited::new(body, 2_000_000)
        .collect()
        .await
        .map_err(|_| anyhow::anyhow!("Invalid request size"))?
        .to_bytes();
    let parse_raw = raw.clone();
    let payload: Value =
        blocking::run(move || serde_json::from_slice(&parse_raw).context("Invalid request JSON"))
            .await?;
    match path.as_str() {
        "/archive/sync" => Ok(json_response(&archive.request_sync(), StatusCode::OK)),
        "/archive/credits" => Ok(json_response(&archive.request_credits(), StatusCode::OK)),
        "/archive/task" | "/archive/recover" => Ok(json_response(
            &archive.request_task(
                text(&payload, "task_id", ""),
                path == "/archive/recover",
                text(&payload, "consent", ""),
            )?,
            StatusCode::OK,
        )),
        "/controls/arm-one" => {
            let value =
                blocking::run(move || engine.arm_one(text(&payload, "consent", ""))).await?;
            Ok(json_response(&value, StatusCode::OK))
        }
        "/controls/cancel-allowance" => {
            let value = blocking::run(move || engine.cancel()).await?;
            Ok(json_response(&value, StatusCode::OK))
        }
        "/v1/chat/completions" | "/chat/completions" => {
            if !is_native_request(&payload) {
                let root = engine.root.clone();
                let s = blocking::run(move || read_settings(&root)).await?;
                let backend = text(&s, "llm_backend", "antigravity");
                if backend == "antigravity" {
                    return crate::antigravity::handle_chat_completion(&payload, &s, engine.root.clone()).await;
                }
                // Ordinary dialogue does not depend on song profile validity.
                // Read only the existing credential-file path; leave its payload intact.
                let key = relay_key(engine.root.clone()).await?;
                return relay(raw, headers, lyrics::client()?, key, OPENROUTER).await;
            }
            let answer = engine.compose(&payload).await?;
            let root = engine.root.clone();
            let s = blocking::run(move || load(&root)).await?;
            let (body, kind) = chat_response(
                &answer,
                flag(&payload, "stream", false),
                text(&s, "selected_model", "native-bard-composer"),
            );
            Ok(response(body, kind, StatusCode::OK))
        }
        "/suno/generate" => Ok(json_response(
            &json!({"error":"Native Suno uses canonical direct HTTPS. This gateway does not generate music. Use the explicit reviewed standalone command only for separately authorized work."}),
            StatusCode::GONE,
        )),
        _ => Ok(json_response(
            &json!({"error":"Not found"}),
            StatusCode::NOT_FOUND,
        )),
    }
}
pub async fn serve(root: PathBuf, port: u16) -> Result<()> {
    // Bind before touching journals; never replace, terminate, or take over an existing server.
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .context(
            "Music port is occupied; keep the existing engine and close Skyrim before switching",
        )?;
    let engine = blocking::run(move || {
        read_settings(&root)?;
        token(&root)?;
        lyrics::client()?; // Initialize TLS/client state once, away from async I/O.
        Engine::new(root)
    })
    .await?;
    let archive = Archive::new(engine.root.clone());
    let archive_worker = tokio::spawn(archive.clone().worker());
    let clock = tokio::spawn(engine.clone().clock_loop());
    println!(
        "{}",
        json!({"event":"listening","runtime":"rust","version":env!("CARGO_PKG_VERSION"),"address":listener.local_addr()?.to_string(),"native_suno_transport":"direct_https"})
    );
    loop {
        tokio::select! {accepted=listener.accept()=>{let(stream,_)=accepted?;let engine=engine.clone();let archive=archive.clone();tokio::spawn(async move{let service=service_fn(move|req|{let engine=engine.clone();let archive=archive.clone();async move{let result=handle(req,engine,archive).await.unwrap_or_else(|_|json_response(&json!({"error":"Music engine stopped this request; inspect local attempt metadata","automatic_retry":false}),StatusCode::BAD_GATEWAY));Ok::<_,Infallible>(result)}});let _=hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(stream),service).await;});},_ = tokio::signal::ctrl_c()=>{clock.abort();archive_worker.abort();break;}}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bard_sse_is_openai_compatible() {
        let (body, kind) = chat_response(
            "Title: T\nStyle: lute\nLyrics:\nHello",
            true,
            "z-ai/glm-5.3-flash",
        );
        assert_eq!(kind, "text/event-stream");
        assert!(body.ends_with("data: [DONE]\n\n"));
        let mut parser = lyrics::SseParser::default();
        let events = parser.feed(body.as_bytes()).unwrap();
        assert_eq!(events[1]["choices"][0]["finish_reason"], "stop");
    }
    #[tokio::test]
    async fn relay_preserves_payload_and_stream_bytes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let payload = Bytes::from_static(
            b"{ \"model\" : \"normal/model\", \"temperature\":0.777, \"stream\":true }",
        );
        let expected = payload.clone();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let n = socket.read(&mut chunk).await.unwrap();
                received.extend_from_slice(&chunk[..n]);
                if received
                    .windows(4)
                    .position(|x| x == b"\r\n\r\n")
                    .is_some_and(|i| received.len() >= i + 4 + expected.len())
                {
                    break;
                }
            }
            let index = received.windows(4).position(|x| x == b"\r\n\r\n").unwrap() + 4;
            assert_eq!(&received[index..], expected.as_ref());
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 19\r\n\r\ndata: unchanged\n\nOK").await.unwrap();
        });
        let result = relay(
            payload,
            hyper::HeaderMap::new(),
            lyrics::client().unwrap(),
            "test-only".into(),
            &format!("http://{addr}"),
        )
        .await
        .unwrap();
        let bytes = result.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), b"data: unchanged\n\nOK");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn pooled_relay_reuses_socket_and_reloads_credentials_without_error_retry() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let accepted = connections.clone();
        let seen = requests.clone();
        let server = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                accepted.fetch_add(1, Ordering::SeqCst);
                let seen = seen.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req: Request<Incoming>| {
                        let seen = seen.clone();
                        async move {
                            let auth = req.headers()["authorization"].to_str().unwrap().to_owned();
                            let title = req.headers()["x-title"].to_str().unwrap().to_owned();
                            let bytes = req.into_body().collect().await.unwrap().to_bytes();
                            let index = {
                                let mut seen = seen.lock().unwrap();
                                seen.push((auth, title, bytes));
                                seen.len()
                            };
                            let mut result = response(
                                "data: original\n\n",
                                "text/event-stream",
                                if index == 2 {
                                    StatusCode::TOO_MANY_REQUESTS
                                } else {
                                    StatusCode::OK
                                },
                            );
                            result
                                .headers_mut()
                                .insert("retry-after", "17".parse().unwrap());
                            result
                                .headers_mut()
                                .insert("x-request-id", "mock-id".parse().unwrap());
                            Ok::<_, Infallible>(result)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(socket), service)
                        .await;
                });
            }
        });
        let fixture = tempfile::tempdir().unwrap();
        let providers = fixture.path().join("Providers.yaml");
        save_json(&fixture.path().join("settings.json"),
            &json!({"selected_model":"invalid song preferences", "openrouter_credentials_file":providers})).unwrap();
        let payload = Bytes::from_static(b"{ \"model\":\"npc/original\", \"stream\":true }");
        for (index, key) in ["sk-or-fixture-first", "sk-or-fixture-rotated"]
            .iter()
            .enumerate()
        {
            std::fs::write(
                &providers,
                format!("providers:\n  - id: openrouter\n    api_key: {key}\n"),
            )
            .unwrap();
            let mut headers = hyper::HeaderMap::new();
            headers.insert("x-title", "Unchanged NPC".parse().unwrap());
            let key = relay_key(fixture.path().to_owned()).await.unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                relay(
                    payload.clone(),
                    headers,
                    lyrics::client().unwrap(),
                    key,
                    &endpoint,
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(
                result.status(),
                if index == 1 {
                    StatusCode::TOO_MANY_REQUESTS
                } else {
                    StatusCode::OK
                }
            );
            assert_eq!(result.headers()["retry-after"], "17");
            assert_eq!(result.headers()["x-request-id"], "mock-id");
            assert_eq!(
                result.into_body().collect().await.unwrap().to_bytes(),
                "data: original\n\n"
            );
        }
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "sequential requests must share the TCP connection"
        );
        let seen = requests.lock().unwrap();
        assert_eq!(seen.len(), 2, "429 must not cause an automatic retry");
        assert_eq!(seen[0].0, "Bearer sk-or-fixture-first");
        assert_eq!(seen[1].0, "Bearer sk-or-fixture-rotated");
        assert!(seen
            .iter()
            .all(|(_, title, raw)| title == "Unchanged NPC" && raw == &payload));
        server.abort();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unfinished_stream_does_not_block_another_relay() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let release = Arc::new(tokio::sync::Notify::new());
        let released = release.clone();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let release = released.clone();
                tokio::spawn(async move {
                    let mut bytes = [0; 4096];
                    let n = socket.read(&mut bytes).await.unwrap();
                    if bytes[..n].starts_with(b"POST /slow ") {
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n6\r\nfirst!\r\n").await.unwrap();
                        release.notified().await;
                        socket.write_all(b"5\r\nlast!\r\n0\r\n\r\n").await.unwrap();
                    } else {
                        socket
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nfast")
                            .await
                            .unwrap();
                    }
                });
            }
        });
        let slow = relay(
            Bytes::from_static(b"{}"),
            hyper::HeaderMap::new(),
            lyrics::client().unwrap(),
            "fixture".into(),
            &format!("{endpoint}/slow"),
        )
        .await
        .unwrap();
        let mut body = slow.into_body();
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(first, "first!"); // No whole-response buffering.
        let fast = tokio::time::timeout(Duration::from_secs(3), async {
            relay(
                Bytes::from_static(b"{}"),
                hyper::HeaderMap::new(),
                lyrics::client().unwrap(),
                "fixture".into(),
                &format!("{endpoint}/fast"),
            )
            .await
            .unwrap()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
        })
        .await;
        release.notify_one();
        assert_eq!(fast.unwrap(), "fast");
        assert_eq!(body.collect().await.unwrap().to_bytes(), "last!");
        server.await.unwrap();
    }
}
