use crate::config::Result;
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};
pub fn request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: Option<&str>,
) -> Result<Value> {
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .map_err(|_| "Local service is not available".to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(if path == "/command" {
            30
        } else {
            4
        })))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let data = body
        .map(|v| serde_json::to_vec(v).unwrap())
        .unwrap_or_default();
    let auth = token
        .map(|s| format!("Authorization: Bearer {}\r\n", s.trim()))
        .unwrap_or_default();
    if auth.contains('\n') && auth.matches('\n').count() != 1 {
        return Err("Malformed local credential".into());
    }
    let head = format!(
        "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        data.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(&data))
        .map_err(|_| "Local request could not be sent".to_string())?;
    let mut raw = Vec::new();
    stream
        .take(2_000_000)
        .read_to_end(&mut raw)
        .map_err(|_| "Local service did not complete its response".to_string())?;
    let n = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("Invalid local service response")?;
    let header = String::from_utf8_lossy(&raw[..n]);
    let status = header
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("500");
    let value: Value = serde_json::from_slice(&raw[n + 4..])
        .map_err(|_| "Local service returned invalid JSON".to_string())?;
    if status != "200" {
        return Err(value["error"]
            .as_str()
            .unwrap_or("Local service rejected the operation")
            .to_owned());
    }
    Ok(value)
}
