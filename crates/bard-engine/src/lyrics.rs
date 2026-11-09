use crate::{blocking, config::*, song::*};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
    time::Duration,
};
use tokio::time::{timeout_at, Instant};

pub fn client() -> Result<reqwest::Client> {
    // Authorization stays on each request, so live key changes never require
    // rebuilding the shared connection pool or leave a stale default header.
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()?;
    // Concurrent first users all return the winning pool. A failed build is not
    // cached; startup can report it without poisoning future process starts.
    let _ = CLIENT.set(client);
    Ok(CLIENT
        .get()
        .expect("shared HTTP client initialized")
        .clone())
}

#[derive(Default)]
pub struct SseParser {
    pending: Vec<u8>,
    data: Vec<String>,
}
impl SseParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Value>> {
        self.pending.extend_from_slice(bytes);
        if self.pending.len() > 2_000_000 {
            bail!("Oversized provider stream event");
        }
        let mut out = vec![];
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line = self.pending.drain(..=end).collect::<Vec<_>>();
            let line = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
            self.line(line, &mut out)?;
        }
        Ok(out)
    }
    fn line(&mut self, line: &str, out: &mut Vec<Value>) -> Result<()> {
        if line.is_empty() {
            self.flush(out)?;
        } else if let Some(data) = line.strip_prefix("data:") {
            self.data.push(data.trim_start().to_owned());
        }
        Ok(())
    }
    fn flush(&mut self, out: &mut Vec<Value>) -> Result<()> {
        if !self.data.is_empty() {
            let data = self.data.join("\n");
            self.data.clear();
            if data != "[DONE]" {
                out.push(serde_json::from_str(&data).context("Invalid provider event JSON")?);
            }
        }
        Ok(())
    }
    pub fn finish(&mut self) -> Result<Vec<Value>> {
        let bytes = std::mem::take(&mut self.pending);
        let mut out = vec![];
        if !bytes.is_empty() {
            self.line(
                std::str::from_utf8(&bytes)?.trim_end_matches('\r'),
                &mut out,
            )?;
        }
        self.flush(&mut out)?;
        Ok(out)
    }
}

fn process_event(
    event: Value,
    evidence: &mut Value,
    answer: &mut String,
    finish: &mut Option<String>,
    started: Instant,
) -> Result<bool> {
    if truthy(&event["error"]) {
        bail!("OpenRouter stream error; no automatic retry");
    }
    for key in ["model", "provider", "id", "usage"] {
        if !event[key].is_null() {
            evidence[key] = event[key].clone();
        }
    }
    let mut first = false;
    if let Some(choices) = event["choices"].as_array() {
        for choice in choices {
            if let Some(reason) = choice["finish_reason"].as_str() {
                *finish = Some(reason.to_owned());
            }
            let delta = &choice["delta"];
            let content = delta["content"].as_str().unwrap_or("");
            let reasoning = delta["reasoning"]
                .as_str()
                .or_else(|| delta["reasoning_content"].as_str())
                .unwrap_or("");
            let details = delta["reasoning_details"]
                .as_array()
                .is_some_and(|a| !a.is_empty());
            if (!content.is_empty() || !reasoning.is_empty() || details)
                && evidence["first_content_seconds"].is_null()
            {
                evidence["first_content_seconds"] = json!(started.elapsed().as_secs_f64());
                evidence["first_content_kind"] = json!(if content.is_empty() {
                    "reasoning"
                } else {
                    "answer"
                });
                first = true;
            }
            evidence["reasoning_characters"] = json!(
                evidence["reasoning_characters"].as_u64().unwrap_or(0)
                    + reasoning.chars().count() as u64
            );
            answer.push_str(content);
            if answer.len() > 2_000_000 {
                bail!("Oversized lyric answer");
            }
        }
    }
    Ok(first)
}

async fn stream_call(
    client: &reqwest::Client,
    endpoint: &str,
    key: &str,
    request: &Value,
    s: &Value,
    evidence: &mut Value,
) -> Result<(Value, String)> {
    let started = Instant::now();
    let first = Duration::from_secs_f64(num(s, "first_response_timeout_seconds", 30.0));
    let total = Duration::from_secs_f64(num(s, "completion_timeout_seconds", 300.0));
    let mut deadline = started + first;
    let response = timeout_at(
        deadline,
        client
            .post(endpoint)
            .bearer_auth(key)
            .header("X-OpenRouter-Title", "Skyrim Bard Composer")
            .json(request)
            .send(),
    )
    .await
    .context("First response timeout")?
    .context("Provider connection failed")?;
    evidence["http_status"] = json!(response.status().as_u16());
    evidence["headers_seconds"] = json!(started.elapsed().as_secs_f64());
    if !response.status().is_success() {
        bail!("OpenRouter HTTP error; no automatic retry");
    }
    let mut stream = response.bytes_stream();
    let mut parser = SseParser::default();
    let mut answer = String::new();
    let mut finish = None;
    loop {
        let next = timeout_at(deadline, stream.next())
            .await
            .context("Provider content deadline exceeded")?;
        match next {
            Some(chunk) => {
                for event in parser.feed(&chunk.context("Provider stream interrupted")?)? {
                    if process_event(event, evidence, &mut answer, &mut finish, started)? {
                        deadline = started + total;
                    }
                }
            }
            None => break,
        }
    }
    for event in parser.finish()? {
        process_event(event, evidence, &mut answer, &mut finish, started)?;
    }
    evidence["finish_reason"] = json!(finish);
    if finish.as_deref() != Some("stop") {
        bail!("OpenRouter output did not finish normally");
    }
    let song = normalize_directions(
        serde_json::from_str(&answer).context("Model answer is not a complete JSON song")?,
    )?;
    evidence["elapsed_seconds"] = json!(started.elapsed().as_secs_f64());
    evidence["success"] = json!(true);
    evidence["song_title"] = song["title"].clone();
    evidence["sung_word_count"] = song["sung_word_count"].clone();
    Ok((song, answer))
}

pub async fn make_draft(
    root: &Path,
    s: &Value,
    context: Value,
    output: Option<&Path>,
) -> Result<PathBuf> {
    let root = root.to_path_buf();
    let s = s.clone();
    let output = output.map(PathBuf::from);
    let preparation_root = root.clone();
    let preparation_settings = s.clone();
    let (context, history_hash, request, destination, mut evidence) = blocking::run(move || {
    let root = &preparation_root;
    let s = &preparation_settings;
    validate(s)?;
    let context = apply_voice(s, context)?;
    let (history, history_hash) = load_history(root, s)?;
    let request = build_request(s, &history, &context)?;
    let destination = output.unwrap_or_else(|| {
        root.join("drafts")
            .join(chrono::Local::now().format("%Y%m%d-%H%M%S-%f").to_string())
    });
    if destination.exists() {
        bail!("Draft destination already exists; use a new directory");
    }
    std::fs::create_dir_all(&destination)?;
    save_json(&destination.join("context.json"), &context)?;
    let evidence_provider = if text(s, "lyrics_provider", "openrouter") == "antigravity" { "antigravity" } else { "openrouter" };
    let evidence = json!({"provider":evidence_provider,"requested_model":request["model"],"model":request["model"],"reasoning_effort":text(s,"reasoning_effort","high"),"history_included":flag(s,"include_history",true),"history_turns":history.as_array().map_or(0,Vec::len),"message_count":request["messages"].as_array().map_or(0,Vec::len),"input_characters":request["messages"].as_array().unwrap().iter().map(|m|m["content"].as_str().unwrap().chars().count()).sum::<usize>(),"first_content_seconds":null,"first_content_kind":null,"reasoning_characters":0,"max_tokens":request["max_tokens"],"temperature":request["temperature"],"first_response_timeout_seconds":num(s,"first_response_timeout_seconds",30.0),"completion_timeout_seconds":num(s,"completion_timeout_seconds",300.0),"new_length_instructions":false,"automatic_retry":false,"suno_submitted":false,"runtime":"rust"});
    Ok((context, history_hash, request, destination, evidence))
    }).await?;
    let started = Instant::now();
    let provider = text(&s, "lyrics_provider", "openrouter").to_owned();
    let provider_name = provider.clone();
    let lyrics_root = root.clone();
    let lyrics_s = s.clone();
    let result = async {
        if provider_name == "antigravity" {
            let sandbox_dir = lyrics_s["antigravity"]["sandbox_dir"]
                .as_str()
                .map(PathBuf::from)
                .unwrap_or_else(|| lyrics_root.join("agy-sandbox"));
            let agy_path = lyrics_s["antigravity"]["agy_path"].as_str().map(str::to_owned);
            let client = crate::antigravity::get_client(sandbox_dir, agy_path);
            let target_model = text(&lyrics_s["antigravity"], "model", crate::antigravity::DEFAULT_ANTIGRAVITY_MODEL);
            let messages = request["messages"].as_array().context("Invalid messages array")?;
            let (system_prompt, prompt_blocks) = crate::antigravity::openai_to_acp(messages)?;
            let session_id = client.new_session(target_model, system_prompt.as_deref()).await?;
            let timeout_secs = num(&lyrics_s, "completion_timeout_seconds", 300.0);
            let (full_text, usage) = client.prompt_buffered(&session_id, prompt_blocks, Duration::from_secs_f64(timeout_secs)).await?;

            let clean_json = if let Some(start) = full_text.find('{') {
                if let Some(end) = full_text.rfind('}') {
                    &full_text[start..=end]
                } else {
                    full_text.as_str()
                }
            } else {
                full_text.as_str()
            };

            let song = normalize_directions(
                serde_json::from_str(clean_json).context("Model answer is not a complete JSON song")?,
            )?;
            evidence["provider"] = json!("antigravity");
            evidence["model"] = json!(target_model);
            evidence["elapsed_seconds"] = json!(started.elapsed().as_secs_f64());
            evidence["success"] = json!(true);
            evidence["song_title"] = song["title"].clone();
            evidence["sung_word_count"] = song["sung_word_count"].clone();
            evidence["usage"] = usage;
            Ok((song, clean_json.to_owned()))
        } else {
            let key_settings = s.clone();
            let key = blocking::run(move || openrouter_key(&key_settings)).await?;
            stream_call(&client()?, OPENROUTER, &key, &request, &s, &mut evidence).await
        }
    }
    .await;
    let (song, raw) = match result {
        Ok(r) => r,
        Err(e) => {
            evidence["success"] = json!(false);
            evidence["elapsed_seconds"] = json!(started.elapsed().as_secs_f64());
            evidence["error"] = json!(e.to_string());
            evidence["failure_type"] = json!("LyricsFailure");
            blocking::run(move || {
                save_json(&destination.join("attempts.json"), &json!([evidence]))
            })
            .await?;
            bail!("Selected lyrics model failed; inspect the saved attempt metadata. No retry or Suno submission occurred");
        }
    };
    blocking::run(move || {
    let s = &s;
    save_json(&destination.join("song.json"), &song)?;
    std::fs::write(destination.join("answer.raw.json"), raw)?;
    save_json(&destination.join("attempts.json"), &json!([evidence]))?;
    save_json(&destination.join("result.json"), &evidence)?;
    save_json(
        &destination.join("request-metadata.json"),
            &json!({"provider":evidence["provider"].as_str().unwrap_or("openrouter"),"model":evidence["model"],"reasoning_effort":text(s,"reasoning_effort","high"),"history_included":flag(s,"include_history",true),"history_turns":evidence["history_turns"],"history_sha256":history_hash,"message_count":evidence["message_count"],"new_length_instructions":false,"voice_control":context["voice_control"],"suno_duration_sent_to_lyrics_model":false,"suno_submitted":false,"runtime":"rust"}),
    )?;
    std::fs::write(destination.join("review.md"),format!("# {}\n\nStyle: {}\n\nVoice: {}\n\n{}\n\nArrangement: {}\n\nModel: {}; thinking: {}; history turns: {}. First content {} s ({}); complete {} s.\n\nNo Suno request has been sent. A paid test needs explicit approval of these lyrics.\n",text(&song,"title",""),text(&song,"style",""),text(&song,"vocal_gender",""),text(&song,"lyrics",""),text(&song,"arrangement_notes",""),text(&evidence,"model",""),text(&evidence,"reasoning_effort",""),evidence["history_turns"],evidence["first_content_seconds"],text(&evidence,"first_content_kind",""),evidence["elapsed_seconds"]))?;
    save_json(
        &root.join("data/latest-draft.json"),
        &json!({"directory":destination,"suno_submitted":false}),
    )?;
    Ok(destination)
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_utf8_and_multiline_sse() {
        let mut p = SseParser::default();
        let data = "data: {\"x\":\"ä\"}\r\n\r\ndata: [DONE]\n\n".as_bytes();
        let mut events = vec![];
        for byte in data {
            events.extend(p.feed(&[*byte]).unwrap());
        }
        assert_eq!(events, vec![json!({"x":"ä"})]);
    }
    #[test]
    fn reasoning_counts_but_is_not_saved() {
        let mut e = json!({"first_content_seconds":null,"reasoning_characters":0});
        let mut answer = String::new();
        let mut finish = None;
        assert!(process_event(
            json!({"choices":[{"delta":{"reasoning":"private-thought"}}]}),
            &mut e,
            &mut answer,
            &mut finish,
            Instant::now()
        )
        .unwrap());
        assert!(answer.is_empty());
        assert_eq!(e["first_content_kind"], "reasoning");
        assert!(!e.to_string().contains("private-thought"));
    }
    #[test]
    fn empty_heartbeat_does_not_start_response() {
        let mut e = json!({"first_content_seconds":null,"reasoning_characters":0});
        assert!(!process_event(
            json!({"choices":[{"delta":{"reasoning_details":[]}}]}),
            &mut e,
            &mut String::new(),
            &mut None,
            Instant::now()
        )
        .unwrap());
    }
    #[tokio::test]
    async fn header_only_response_cannot_extend_first_deadline() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = [0; 4096];
            let _ = socket.read(&mut data).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let mut e = json!({"first_content_seconds":null});
        let r = stream_call(
            &client().unwrap(),
            &format!("http://{addr}"),
            "mock-key",
            &json!({}),
            &json!({"first_response_timeout_seconds":0.04,"completion_timeout_seconds":1}),
            &mut e,
        )
        .await;
        assert!(r.is_err());
        task.abort();
    }
    async fn reasoning_mock(total: f64, final_delay: u64) -> (Result<(Value, String)>, Value) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            let thinking = format!(
                "data: {}\n\n",
                json!({"choices":[{"delta":{"reasoning":"not-retained"}}]})
            );
            socket
                .write_all(format!("{:x}\r\n{}\r\n", thinking.len(), thinking).as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(final_delay)).await;
            let song = json!({"title":"Mock song","style":"acoustic lute","lyrics":"The lantern shines","vocal_gender":"m","arrangement_notes":"quiet"});
            let final_event = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"delta":{"content":song.to_string()},"finish_reason":"stop"}]})
            );
            let _ = socket
                .write_all(
                    format!("{:x}\r\n{}\r\n0\r\n\r\n", final_event.len(), final_event).as_bytes(),
                )
                .await;
        });
        let mut evidence = json!({"first_content_seconds":null,"reasoning_characters":0});
        let result = stream_call(
            &client().unwrap(),
            &format!("http://{addr}"),
            "mock-only",
            &json!({}),
            &json!({"first_response_timeout_seconds":0.05,"completion_timeout_seconds":total}),
            &mut evidence,
        )
        .await;
        task.abort();
        (result, evidence)
    }
    #[tokio::test]
    async fn reasoning_unlocks_completion_deadline_without_being_stored() {
        let (result, evidence) = reasoning_mock(1.0, 100).await;
        let (song, raw) = result.unwrap();
        assert_eq!(song["title"], "Mock song");
        assert_eq!(evidence["first_content_kind"], "reasoning");
        assert!(!raw.contains("not-retained"));
        assert!(!evidence.to_string().contains("not-retained"));
    }
    #[tokio::test]
    async fn reasoning_cannot_extend_absolute_total_deadline() {
        let (result, evidence) = reasoning_mock(0.1, 300).await;
        assert!(result.is_err());
        assert_eq!(evidence["first_content_kind"], "reasoning");
    }
}
