use crate::{config::*, lyrics};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

pub fn verified_payload(draft: &Path) -> Result<Value> {
    let approval = read_json(&draft.join("suno-approval-required.json"))?;
    let payload = read_json(&draft.join("suno-request.review.json"))?;
    if approval["approved"] != true || !truthy(&approval["human_authorization"]) {
        bail!("Explicit human approval of this exact standalone request is required");
    }
    if approval["request_sha256"] != digest(&payload) {
        bail!("Payload changed after approval");
    }
    if payload["customMode"] != true {
        bail!("Expected reviewed custom request");
    }
    if !payload["duration"].is_null()
        && !payload["duration"]
            .as_u64()
            .is_some_and(|n| (120..=179).contains(&n))
    {
        bail!("Invalid reviewed duration");
    }
    Ok(payload)
}
pub async fn submit_approved(draft: &Path, s: &Value) -> Result<Value> {
    let payload = verified_payload(draft)?;
    let key = suno_key(s)?;
    let journal = draft.join("suno-submission.json");
    create_journal(
        &journal,
        &json!({"state":"submitting_or_unknown","request_sha256":digest(&payload),"started_at":crate::engine::now(),"note":"No automatic retry. Reconcile provider history if task ID is unknown."}),
    )?;
    let response = lyrics::client()?
        .post(format!("{SUNO}/generate"))
        .bearer_auth(key)
        .timeout(Duration::from_secs(30))
        .json(&payload)
        .send()
        .await
        .context("Uncertain Suno submission; never retry automatically")?;
    if !response.status().is_success() {
        bail!("Suno submission failed or is uncertain; journal retained and no retry");
    }
    let body: Value = response
        .json()
        .await
        .context("Suno response malformed; outcome uncertain")?;
    let task = body["data"]["taskId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("Suno did not confirm a task ID; do not retry")?;
    if body["code"] != 200 {
        bail!("Suno did not confirm submission; do not retry");
    }
    let record = json!({"state":"submitted","task_id":task,"request_sha256":digest(&payload)});
    save_json(&journal, &record)?;
    Ok(record)
}
pub async fn poll_existing(draft: &Path, s: &Value, maximum: Duration) -> Result<Value> {
    let journal = read_json(&draft.join("suno-submission.json"))?;
    let task = journal["task_id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("No known task ID; reconcile provider history without resubmitting")?;
    let key = suno_key(s)?;
    let client = lyrics::client()?;
    let deadline = tokio::time::Instant::now() + maximum;
    loop {
        let response = tokio::time::timeout_at(
            deadline,
            client
                .get(format!("{SUNO}/generate/record-info"))
                .query(&[("taskId", task)])
                .bearer_auth(&key)
                .timeout(Duration::from_secs(30))
                .send(),
        )
        .await
        .context("Polling timed out; resume the same known task later")??;
        if !response.status().is_success() {
            bail!("Existing task polling failed; no new generation");
        }
        let body: Value = tokio::time::timeout_at(deadline, response.json()).await??;
        let state = text(&body["data"], "status", "");
        if state == "SUCCESS" {
            if body["data"]["response"]["sunoData"]
                .as_array()
                .is_none_or(|v| v.is_empty())
            {
                bail!("Completed task has no tracks");
            }
            save_json(&draft.join("suno-result.json"), &body)?;
            return Ok(body);
        }
        if state.contains("FAIL") || state.contains("ERROR") {
            bail!("Existing Suno task failed; no new generation");
        }
        tokio::time::timeout_at(deadline, tokio::time::sleep(Duration::from_secs(5)))
            .await
            .context("Polling timed out; resume existing task later")?;
    }
}
fn hidden_command(program: &str) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command
}
pub fn audio_url(track: &Value) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(track["audioUrl"].as_str().context("Missing audio URL")?)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("Audio must use credential-free HTTPS");
    }
    let host = url.host_str().unwrap();
    if ["localhost", "127.0.0.1", "::1", "[::1]"].contains(&host) {
        bail!("Loopback audio URL rejected");
    }
    Ok(url)
}
async fn download(track: &Value, path: &Path) -> Result<()> {
    let url = audio_url(track)?;
    let response = lyrics::client()?
        .get(url)
        .timeout(Duration::from_secs(90))
        .send()
        .await?;
    if response.status() != 200 {
        bail!("Audio download failed; no redirect or new generation");
    }
    if response
        .content_length()
        .is_some_and(|n| n > 100 * 1024 * 1024)
    {
        bail!("Audio exceeds size limit");
    }
    let tmp = path.with_extension("mp3.partial");
    let mut file = std::fs::File::create(&tmp)?;
    let mut count = 0;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        count += chunk.len();
        if count > 100 * 1024 * 1024 {
            bail!("Audio exceeds size limit");
        }
        file.write_all(&chunk)?;
    }
    if count == 0 {
        bail!("Empty audio");
    }
    file.sync_all()?;
    drop(file);
    std::fs::rename(tmp, path)?;
    Ok(())
}
pub async fn download_existing(draft: &Path, s: &Value) -> Result<Value> {
    // Fail before network if the verification tools are unavailable.
    for tool in ["ffprobe", "ffmpeg"] {
        if !hidden_command(tool)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .context("ffmpeg and ffprobe are required for full audio verification")?
            .success()
        {
            bail!("Audio verification tool failed");
        }
    }
    let body = if draft.join("suno-result.json").exists() {
        read_json(&draft.join("suno-result.json"))?
    } else {
        poll_existing(draft, s, Duration::from_secs(600)).await?
    };
    if body["data"]["status"] != "SUCCESS" {
        bail!("Saved Suno result has not completed");
    }
    let tracks = body["data"]["response"]["sunoData"]
        .as_array()
        .filter(|v| !v.is_empty())
        .context("Missing completed variants")?;
    let song = read_json(&draft.join("song.json"))?;
    let payload = read_json(&draft.join("suno-request.review.json"))?;
    let stem = regex::Regex::new(r"[^A-Za-z0-9]+")?
        .replace_all(text(&song, "title", "Bard Song"), "-")
        .trim_matches('-')
        .chars()
        .take(100)
        .collect::<String>();
    let stem = if stem.is_empty() { "Bard-Song" } else { &stem };
    let mut records = vec![];
    for (index, track) in tracks.iter().enumerate() {
        let variant = index + 1;
        let path = draft.join(format!("{stem}-{variant}.mp3"));
        if !path.exists() {
            download(track, &path).await?;
        }
        let probe = hidden_command("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration,format_name,size:stream=codec_name,sample_rate,channels",
                "-of",
                "json",
            ])
            .arg(&path)
            .output()?;
        if !probe.status.success() {
            bail!("Audio probe failed; original saved for review");
        }
        let details: Value = serde_json::from_slice(&probe.stdout)?;
        let decode = hidden_command("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-f", "null", "-"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        let seconds = details["format"]["duration"]
            .as_str()
            .context("Missing measured duration")?
            .parse::<f64>()?;
        let requested = payload["duration"].as_f64();
        records.push(json!({"variant":variant,"path":path,"track_id":track["id"],"probe":details,"full_decode_passed":decode.success(),"sha256":sha(&std::fs::read(&path)?),"duration_seconds":seconds,"requested_duration_seconds":requested,"under_three_minutes":seconds<180.0,"within_requested_duration":requested.map(|r|seconds<=r+0.1),"original_intact":true}));
    }
    save_json(&draft.join("audio-verification.json"), &json!(records))?;
    if records.iter().any(|r| r["full_decode_passed"] != true) {
        bail!("One or more audio files failed full decode; originals retained");
    }
    Ok(
        json!({"variants_saved":records.len(),"verification":draft.join("audio-verification.json"),"native_import_performed":false,"import_note":"Use the authorized SkyrimNet dashboard import. HTTP request gate is not bypassed.","new_generation_submitted":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approval_hash_is_bound_to_payload() {
        let d = tempfile::tempdir().unwrap();
        let p = json!({"customMode":true,"title":"First"});
        save_json(&d.path().join("suno-request.review.json"), &p).unwrap();
        save_json(&d.path().join("suno-approval-required.json"),&json!({"approved":true,"human_authorization":"Explicit test approval","request_sha256":digest(&p)})).unwrap();
        assert!(verified_payload(d.path()).is_ok());
        save_json(
            &d.path().join("suno-request.review.json"),
            &json!({"customMode":true,"title":"Changed"}),
        )
        .unwrap();
        assert!(verified_payload(d.path()).is_err());
    }
    #[test]
    fn journal_create_is_exclusive() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("journal.json");
        create_journal(&p, &json!({"state":"unknown"})).unwrap();
        assert!(create_journal(&p, &json!({"state":"retry"})).is_err());
        assert_eq!(read_json(&p).unwrap()["state"], "unknown");
    }
    #[test]
    fn audio_url_rejects_http_and_credentials() {
        for u in [
            "http://example.com/a.mp3",
            "https://user:key@example.com/a.mp3",
            "https://127.0.0.1/a.mp3",
        ] {
            assert!(audio_url(&json!({"audioUrl":u})).is_err());
        }
    }
}
