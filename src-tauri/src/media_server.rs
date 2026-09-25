//! Serves local video files to the webview over http://127.0.0.1.
//!
//! Tauri's `asset:` protocol answers every range request on the UI thread through WebView2's
//! custom-scheme bridge; measured on a USB TeslaCam drive it delivered 0.8-3.5 MB/s for files
//! not yet in the OS cache, below the ~4.6 MB/s six cameras need, so playback kept stopping
//! to wait for data. Plain HTTP goes through Chromium's network stack and streamed the same
//! files at over 100 MB/s.
//!
//! Only loopback connections are accepted, and every URL carries a per-launch random token so
//! other local software or web pages cannot read files through it.

use std::collections::hash_map::RandomState;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

const COPY_BUFFER: usize = 1024 * 1024;

pub struct MediaServer {
    pub base_url: String,
}

pub fn start() -> std::io::Result<MediaServer> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let token = random_token();
    let base_url = format!("http://127.0.0.1:{port}/{token}/");

    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let token = token.clone();
            thread::spawn(move || {
                let _ = handle_connection(stream, &token);
            });
        }
    });

    Ok(MediaServer { base_url })
}

fn random_token() -> String {
    // RandomState is seeded from the OS RNG, which is all a per-launch secret needs
    (0..2)
        .map(|i| {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u64(i);
            format!("{:016x}", hasher.finish())
        })
        .collect()
}

struct Request {
    method: String,
    path: String,
    range: Option<String>,
    keep_alive: bool,
}

fn read_request(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let http10 = parts.next().map(|v| v == "HTTP/1.0").unwrap_or(false);

    let mut range = None;
    let mut keep_alive = !http10;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            return Ok(None);
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "range" => range = Some(value.to_string()),
                "connection" => keep_alive = !value.eq_ignore_ascii_case("close"),
                _ => {}
            }
        }
    }
    Ok(Some(Request { method, path, range, keep_alive }))
}

fn handle_connection(stream: TcpStream, token: &str) -> std::io::Result<()> {
    let _ = stream.set_nodelay(true);
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    while let Some(request) = read_request(&mut reader)? {
        let keep_alive = request.keep_alive;
        respond(&mut writer, token, request)?;
        if !keep_alive {
            break;
        }
    }
    Ok(())
}

const CORS: &str = "Access-Control-Allow-Origin: *\r\n\
Access-Control-Allow-Headers: Range\r\n\
Access-Control-Allow-Methods: GET, HEAD, OPTIONS\r\n\
Access-Control-Allow-Private-Network: true\r\n\
Access-Control-Expose-Headers: Content-Range, Content-Length, Accept-Ranges\r\n";

fn simple_response(writer: &mut TcpStream, status: &str) -> std::io::Result<()> {
    write!(writer, "HTTP/1.1 {status}\r\n{cors}Content-Length: 0\r\n\r\n", cors = CORS)
}

fn respond(writer: &mut TcpStream, token: &str, request: Request) -> std::io::Result<()> {
    if request.method == "OPTIONS" {
        return simple_response(writer, "204 No Content");
    }
    if request.method != "GET" && request.method != "HEAD" {
        return simple_response(writer, "405 Method Not Allowed");
    }

    let prefix = format!("/{token}/");
    let Some(encoded) = request.path.strip_prefix(&prefix) else {
        return simple_response(writer, "403 Forbidden");
    };
    let encoded = encoded.split('?').next().unwrap_or("");
    let path = percent_encoding::percent_decode_str(encoded).decode_utf8_lossy().to_string();

    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(_) => return simple_response(writer, "404 Not Found"),
    };
    let len = file.metadata()?.len();

    let (status, start, end) = match request.range.as_deref().and_then(|r| parse_range(r, len)) {
        Some((start, end)) => ("206 Partial Content", start, end),
        None if request.range.is_some() && len > 0 => {
            return write!(
                writer,
                "HTTP/1.1 416 Range Not Satisfiable\r\n{cors}Content-Range: bytes */{len}\r\nContent-Length: 0\r\n\r\n",
                cors = CORS
            );
        }
        None => ("200 OK", 0, len.saturating_sub(1)),
    };
    let body_len = if len == 0 { 0 } else { end - start + 1 };

    let mut head = format!(
        "HTTP/1.1 {status}\r\n{cors}Content-Type: {}\r\nAccept-Ranges: bytes\r\nContent-Length: {body_len}\r\nCache-Control: no-store\r\n",
        mime_type(&path),
        cors = CORS
    );
    if status.starts_with("206") {
        head.push_str(&format!("Content-Range: bytes {start}-{end}/{len}\r\n"));
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;

    if request.method == "HEAD" || body_len == 0 {
        return Ok(());
    }

    // Stream the range; when the player moves on it closes the socket and the write fails
    file.seek(SeekFrom::Start(start))?;
    let mut remaining = body_len;
    let mut buffer = vec![0u8; COPY_BUFFER];
    while remaining > 0 {
        let want = remaining.min(COPY_BUFFER as u64) as usize;
        let read = file.read(&mut buffer[..want])?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }
    writer.flush()
}

// "bytes=start-end", "bytes=start-" or "bytes=-suffix"; one range only
fn parse_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let spec = header.trim().strip_prefix("bytes=")?.split(',').next()?.trim();
    let (first, last) = spec.split_once('-')?;
    if len == 0 {
        return None;
    }
    let (start, end) = if first.is_empty() {
        let suffix: u64 = last.parse().ok()?;
        if suffix == 0 {
            return None;
        }
        (len.saturating_sub(suffix), len - 1)
    } else {
        let start: u64 = first.parse().ok()?;
        let end = if last.is_empty() { len - 1 } else { last.parse::<u64>().ok()?.min(len - 1) };
        (start, end)
    };
    if start > end || start >= len {
        return None;
    }
    Some((start, end))
}

fn mime_type(path: &str) -> &'static str {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".mp4") || lower.ends_with(".m4v") {
        "video/mp4"
    } else if lower.ends_with(".webm") {
        "video/webm"
    } else if lower.ends_with(".mov") {
        "video/quicktime"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else {
        "application/octet-stream"
    }
}
