//! A `Read + Seek` adapter over an HTTP resource, backed by Range requests —
//! this is the piece that makes real streaming playback possible: `rodio`'s
//! `Decoder` requires `Seek` (Symphonia probes the container format and seeks
//! within it), but a plain HTTP response body only supports sequential
//! reads. Seeking here means dropping the current connection and reissuing a
//! fresh ranged GET lazily on the next read — no seek is "free", but no seek
//! requires downloading more than the bytes actually needed either, which is
//! what makes this genuinely streaming rather than "buffer the whole file
//! first."
//!
//! Gracefully degrades if the server doesn't honor `Range` (returns 200
//! instead of 206): falls back to reading-and-discarding up to the target
//! offset from a fresh full-content response, so playback stays *correct*
//! even when it can't be as cheap.

use std::io::{self, Read, Seek, SeekFrom};

use reqwest::blocking::Client;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE};

pub struct RangeReader {
    client: Client,
    url: String,
    /// Absolute stream position the next `read()` should return bytes from.
    pos: u64,
    /// Total resource length, learned from the first response's
    /// `Content-Range`/`Content-Length` header. `None` until then, which only
    /// matters for `SeekFrom::End` — sequential playback never needs it.
    len: Option<u64>,
    /// The currently-open response body, or `None` if a fresh request must
    /// be issued before the next read (set by `seek`).
    body: Option<reqwest::blocking::Response>,
    /// Set when the server ignored our `Range` header and gave us the whole
    /// resource from byte 0 instead — this many bytes must be read and
    /// discarded before returned data is actually at `pos`.
    pending_skip: u64,
}

impl RangeReader {
    pub fn new(client: Client, url: impl Into<String>) -> Self {
        Self {
            client,
            url: url.into(),
            pos: 0,
            len: None,
            body: None,
            pending_skip: 0,
        }
    }

    /// Total resource length, if known yet (populated after the first byte
    /// is read). Exposed so the playback engine can report track duration
    /// progress even before the whole file is known-seekable-to-end.
    pub fn known_len(&self) -> Option<u64> {
        self.len
    }

    fn ensure_open(&mut self) -> io::Result<()> {
        if self.body.is_some() {
            return Ok(());
        }
        let resp = self
            .client
            .get(&self.url)
            .header(RANGE, format!("bytes={}-", self.pos))
            .send()
            .map_err(to_io_error)?;

        match resp.status().as_u16() {
            206 => {
                if self.len.is_none() {
                    self.len = content_range_total(resp.headers().get(CONTENT_RANGE));
                }
                self.pending_skip = 0;
                self.body = Some(resp);
                Ok(())
            }
            200 => {
                // Server doesn't support Range — we got the whole thing from
                // byte 0. Discard up to `pos` bytes as this body is read.
                if self.len.is_none() {
                    self.len = resp
                        .headers()
                        .get(CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse().ok());
                }
                self.pending_skip = self.pos;
                self.body = Some(resp);
                Ok(())
            }
            status => Err(io::Error::other(
                format!("unexpected status {status} streaming {}", self.url),
            )),
        }
    }
}

impl Read for RangeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            self.ensure_open()?;
            let body = self.body.as_mut().expect("just ensured open");

            if self.pending_skip > 0 {
                // Discard-read in bounded chunks rather than allocating a
                // `pending_skip`-sized buffer up front.
                let mut sink = [0u8; 8192];
                let want = self.pending_skip.min(sink.len() as u64) as usize;
                let n = body.read(&mut sink[..want]).map_err(to_io_error_from_reqwest_read)?;
                if n == 0 {
                    // Resource shorter than expected — nothing left to skip
                    // or read; treat as EOF rather than looping forever.
                    self.pending_skip = 0;
                    return Ok(0);
                }
                self.pending_skip -= n as u64;
                continue;
            }

            let n = body.read(buf)?;
            if n == 0 {
                // This connection is exhausted. If we truly reached the end
                // of the resource, subsequent reads should also return 0;
                // dropping the body just means the next read reopens (cheap
                // no-op GET returning 200/206 with no bytes) rather than a
                // special-cased "at EOF" flag.
                self.body = None;
                return Ok(0);
            }
            self.pos += n as u64;
            return Ok(n);
        }
    }
}

impl Seek for RangeReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(delta) => offset(self.pos, delta)?,
            SeekFrom::End(delta) => {
                let len = self.len.ok_or_else(|| {
                    io::Error::other(
                        "cannot seek from end: resource length not yet known",
                    )
                })?;
                offset(len, delta)?
            }
        };
        if target != self.pos {
            // Drop the open connection; ensure_open() lazily reissues a
            // ranged request for the new position on the next read().
            self.body = None;
            self.pending_skip = 0;
            self.pos = target;
        }
        Ok(self.pos)
    }
}

fn offset(base: u64, delta: i64) -> io::Result<u64> {
    if delta >= 0 {
        base.checked_add(delta as u64)
    } else {
        base.checked_sub(delta.unsigned_abs())
    }
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "seek out of bounds"))
}

/// Parses a `Content-Range: bytes 0-999/1000` header value into the total
/// (the part after `/`).
fn content_range_total(header: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    let v = header?.to_str().ok()?;
    let total = v.rsplit('/').next()?;
    total.parse().ok()
}

fn to_io_error(e: reqwest::Error) -> io::Error {
    io::Error::other(e)
}

fn to_io_error_from_reqwest_read(e: io::Error) -> io::Error {
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;

    /// A tiny single-file HTTP/1.1 server that honors `Range` requests
    /// against an in-memory byte buffer, so `RangeReader` can be tested
    /// against real sockets without a real Navidrome instance.
    struct TestServer {
        addr: std::net::SocketAddr,
        request_count: Arc<AtomicUsize>,
    }

    fn spawn_test_server(data: &'static [u8], honor_range: bool) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let request_count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&request_count);

        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                counter.fetch_add(1, Ordering::SeqCst);

                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let range = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|l| l.split("bytes=").nth(1))
                    .map(|s| s.trim());

                let (status, body, extra_headers) = match (honor_range, range) {
                    (true, Some(spec)) => {
                        let start: usize = spec.trim_end_matches('-').parse().unwrap_or(0);
                        let body = &data[start.min(data.len())..];
                        (
                            "206 Partial Content",
                            body,
                            format!(
                                "Content-Range: bytes {}-{}/{}\r\n",
                                start,
                                data.len().saturating_sub(1),
                                data.len()
                            ),
                        )
                    }
                    _ => ("200 OK", data, String::new()),
                };

                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body);
            }
        });

        // Give the listener a moment to be ready for connections.
        thread::sleep(std::time::Duration::from_millis(20));

        TestServer {
            addr,
            request_count,
        }
    }

    fn url(server: &TestServer) -> String {
        format!("http://{}/track", server.addr)
    }

    const DATA: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

    #[test]
    fn sequential_read_returns_full_resource() {
        let server = spawn_test_server(DATA, true);
        let mut reader = RangeReader::new(Client::new(), url(&server));
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, DATA);
    }

    #[test]
    fn seek_forward_skips_via_range_request() {
        let server = spawn_test_server(DATA, true);
        let mut reader = RangeReader::new(Client::new(), url(&server));
        reader.seek(SeekFrom::Start(10)).unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, &DATA[10..]);
    }

    #[test]
    fn seek_backward_after_reading_ahead_works() {
        let server = spawn_test_server(DATA, true);
        let mut reader = RangeReader::new(Client::new(), url(&server));
        let mut first5 = [0u8; 5];
        reader.read_exact(&mut first5).unwrap();
        assert_eq!(&first5, &DATA[..5]);

        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, DATA);
    }

    #[test]
    fn seek_from_end_uses_learned_length() {
        let server = spawn_test_server(DATA, true);
        let mut reader = RangeReader::new(Client::new(), url(&server));
        // Length is only known after the first response — read one byte first.
        let mut one = [0u8; 1];
        reader.read_exact(&mut one).unwrap();
        assert_eq!(reader.known_len(), Some(DATA.len() as u64));

        reader.seek(SeekFrom::End(-4)).unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, &DATA[DATA.len() - 4..]);
    }

    #[test]
    fn degrades_gracefully_when_server_ignores_range() {
        let server = spawn_test_server(DATA, false);
        let mut reader = RangeReader::new(Client::new(), url(&server));
        reader.seek(SeekFrom::Start(15)).unwrap();
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        // Correctness holds even though the server can't seek server-side —
        // we discard the first 15 bytes of the full response ourselves.
        assert_eq!(out, &DATA[15..]);
    }

    #[test]
    fn seeking_to_the_same_position_reuses_the_open_connection() {
        let server = spawn_test_server(DATA, true);
        let mut reader = RangeReader::new(Client::new(), url(&server));
        let mut one = [0u8; 1];
        reader.read_exact(&mut one).unwrap();
        let requests_before = server.request_count.load(Ordering::SeqCst);

        reader.seek(SeekFrom::Start(1)).unwrap(); // no-op: already at pos 1
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();

        assert_eq!(
            server.request_count.load(Ordering::SeqCst),
            requests_before,
            "seeking to the current position must not reopen the connection"
        );
        assert_eq!(rest, &DATA[1..]);
    }
}
