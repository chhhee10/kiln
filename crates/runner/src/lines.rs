//! JSON-lines framing helpers.

use serde::Serialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Max bytes kept from one line; the remainder of an over-long line is
/// discarded (harness tool outputs can be huge).
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Reads `\n`-terminated lines with a length cap. Cancellation safe: partial
/// data is kept in `self` between polls.
pub struct LineReader<R> {
    inner: R,
    partial: Vec<u8>,
    overflow: bool,
    cap: usize,
}

impl<R: AsyncBufRead + Unpin> LineReader<R> {
    pub fn new(inner: R) -> Self {
        Self::with_cap(inner, MAX_LINE_BYTES)
    }

    pub fn with_cap(inner: R, cap: usize) -> Self {
        Self {
            inner,
            partial: Vec::new(),
            overflow: false,
            cap,
        }
    }

    fn take(&mut self) -> String {
        let s = String::from_utf8_lossy(&self.partial).into_owned();
        self.partial.clear();
        let truncated = std::mem::take(&mut self.overflow);
        let s = s.strip_suffix('\r').map(str::to_string).unwrap_or(s);
        if truncated {
            format!("{s}… [line truncated]")
        } else {
            s
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        let room = self.cap.saturating_sub(self.partial.len());
        if bytes.len() > room {
            self.partial.extend_from_slice(&bytes[..room]);
            self.overflow = true;
        } else {
            self.partial.extend_from_slice(bytes);
        }
    }

    /// Next line without the terminator; `None` at EOF.
    pub async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        loop {
            let buf = self.inner.fill_buf().await?;
            if buf.is_empty() {
                return Ok(if self.partial.is_empty() && !self.overflow {
                    None
                } else {
                    Some(self.take())
                });
            }
            if let Some(i) = buf.iter().position(|b| *b == b'\n') {
                let chunk = buf[..i].to_vec();
                self.inner.consume(i + 1);
                self.push(&chunk);
                return Ok(Some(self.take()));
            }
            let chunk = buf.to_vec();
            let n = chunk.len();
            self.inner.consume(n);
            self.push(&chunk);
        }
    }
}

/// Serialize `msg` as one line and flush.
pub async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    msg: &T,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(msg).map_err(std::io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn reads_lines_with_cap() {
        let data = b"one\r\ntwo\nthreeeeeeeeee\n\xff\xfe\nlast".to_vec();
        let mut r = LineReader::with_cap(BufReader::with_capacity(4, &data[..]), 6);
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("one"));
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("two"));
        assert_eq!(
            r.next_line().await.unwrap().as_deref(),
            Some("threee… [line truncated]")
        );
        assert_eq!(
            r.next_line().await.unwrap().as_deref(),
            Some("\u{fffd}\u{fffd}")
        );
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("last"));
        assert_eq!(r.next_line().await.unwrap(), None);
    }
}
