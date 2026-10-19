//! Minimal raw-HTTP SSE fixture server for adapter tests. Serves a canned response per
//! request; one accept loop until the handle is dropped. No HTTP framework — bytes in,
//! bytes out — so tests exercise the real transport path.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct Fixture {
    pub base_url: String,
}

impl Fixture {
    /// Spawn a fixture server on an OS-assigned port.
    pub async fn start(responses: Vec<(u16, impl Into<String>, Vec<u8>)>) -> Fixture {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture");
        let addr = listener.local_addr().expect("addr");
        let responses = Arc::new(
            responses
                .into_iter()
                .map(|(s, ct, b)| (s, ct.into(), b))
                .collect::<Vec<_>>(),
        );
        let server_responses = responses.clone();
        tokio::spawn(async move {
            let mut i = 0usize;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let resp = server_responses
                    .get(i)
                    .unwrap_or_else(|| server_responses.last().expect("nonempty"))
                    .clone();
                i += 1;
                tokio::spawn(serve(stream, resp));
            }
        });
        Fixture {
            base_url: format!("http://{}", addr),
        }
    }
}

async fn serve(mut stream: TcpStream, (status, content_type, body): (u16, String, Vec<u8>)) {
    // Read the request head (we don't need to parse it; drain until blank line).
    let mut buf = vec![0u8; 8192];
    let _ = stream.read(&mut buf).await;
    let reason = if status == 200 { "OK" } else { "Error" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    // Write the head, then the body in deliberately small pieces to exercise the
    // incremental SSE parser across chunk boundaries.
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.flush().await;
    for piece in body.chunks(7) {
        let _ = stream.write_all(piece).await;
        let _ = stream.flush().await;
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    let _ = stream.flush().await;
    // hold the connection a moment so the client sees EOF cleanly
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
}

pub fn sse_body(frames: &[String]) -> Vec<u8> {
    let mut out = String::new();
    for f in frames {
        out.push_str("data: ");
        out.push_str(f);
        out.push_str("\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    out.into_bytes()
}
