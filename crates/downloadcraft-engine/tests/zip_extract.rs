//! One file out of a remote ZIP: only its bytes are fetched and inflated.

use std::io::Write;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use downloadcraft_engine::zip::{
    extract_remote, extract_remote_with_limits, list_remote, list_remote_with_limits, ZipLimits,
};
use downloadcraft_engine::RequestFn;

/// A ZIP with one stored and one deflated file.
fn make_zip(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let (mut out, mut dir) = (Vec::new(), Vec::new());
    for (name, data, deflate) in files {
        let packed = if *deflate {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(data).unwrap();
            e.finish().unwrap()
        } else {
            data.to_vec()
        };
        let method: u16 = if *deflate { 8 } else { 0 };
        let local = out.len() as u32;
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&[20, 0, 0, 0]);
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&[0; 8]);
        out.extend_from_slice(&(packed.len() as u32).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&3u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        // A local extra field the directory does not have.
        out.extend_from_slice(&[1, 2, 3]);
        out.extend_from_slice(&packed);
        dir.extend_from_slice(b"PK\x01\x02");
        dir.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
        dir.extend_from_slice(&method.to_le_bytes());
        dir.extend_from_slice(&[0; 8]);
        dir.extend_from_slice(&(packed.len() as u32).to_le_bytes());
        dir.extend_from_slice(&(data.len() as u32).to_le_bytes());
        dir.extend_from_slice(&(name.len() as u16).to_le_bytes());
        dir.extend_from_slice(&[0; 12]);
        dir.extend_from_slice(&local.to_le_bytes());
        dir.extend_from_slice(name.as_bytes());
    }
    let at = out.len() as u32;
    let n = files.len() as u16;
    out.extend_from_slice(&dir);
    out.extend_from_slice(b"PK\x05\x06");
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&(dir.len() as u32).to_le_bytes());
    out.extend_from_slice(&at.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

/// Serves `body` with Range support, one request per connection.
async fn serve(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/a.zip", listener.local_addr().unwrap());
    let body = Arc::new(body);
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let body = body.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if sock.read(&mut b).await.unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(b[0]);
                }
                let text = String::from_utf8_lossy(&head).to_lowercase();
                let range = text.lines().find_map(|l| l.strip_prefix("range: bytes=")).unwrap();
                let (s, e) = range.trim().split_once('-').unwrap();
                let s: usize = s.parse().unwrap();
                let e: usize = e.parse::<usize>().unwrap().min(body.len() - 1);
                let part = &body[s..=e];
                let reply = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {s}-{e}/{}\r\nConnection: close\r\n\r\n",
                    part.len(),
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.write_all(part).await;
            });
        }
    });
    base
}

fn unique_dir(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("dce-zip-{}-{tag}-{nanos}", std::process::id()))
}

#[tokio::test]
async fn a_lying_entry_size_cannot_expand_past_its_declared_bytes() {
    // 2 MiB of zeros compress to a few hundred bytes, but the directory says
    // the entry is only 1 KiB: extraction must stop at 1 KiB, not fill disk.
    let data = vec![0u8; 2 * 1024 * 1024];
    let mut zip = make_zip(&[("bomb.bin", &data, true)]);
    let dir_at = zip.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    zip[dir_at + 24..dir_at + 28].copy_from_slice(&1024u32.to_le_bytes());
    let url = serve(zip).await;
    let client = reqwest::Client::new();
    let request: RequestFn = Arc::new(move |u: &str| client.get(u));
    let entries = list_remote(&request, &url).await.unwrap();
    assert_eq!(entries[0].size, 1024);

    let dir = unique_dir("lying");
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("bomb.bin");
    std::fs::write(&dest, b"old bytes").unwrap();
    let err = extract_remote(&request, &url, &entries[0], &dest).await.unwrap_err();
    assert!(err.to_string().contains("limit"), "{err}");
    assert_eq!(std::fs::read(&dest).unwrap(), b"old bytes", "old file must survive");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".dczip-"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left behind: {leftovers:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_huge_directory_claim_is_refused_before_it_is_fetched() {
    let mut zip = make_zip(&[("a.txt", b"hello", false)]);
    let eocd = zip.windows(4).rposition(|w| w == b"PK\x05\x06").unwrap();
    zip[eocd + 12..eocd + 16].copy_from_slice(&u32::MAX.to_le_bytes());
    let url = serve(zip).await;
    let client = reqwest::Client::new();
    let request: RequestFn = Arc::new(move |u: &str| client.get(u));
    let err = list_remote_with_limits(&request, &url, ZipLimits::default()).await.unwrap_err();
    assert!(err.to_string().contains("directory is"), "{err}");
}

#[tokio::test]
async fn one_file_is_fetched_out_of_a_remote_zip() {
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 7) as u8).collect();
    let zip = make_zip(&[("readme.txt", b"hello zip", false), ("data/big.bin", &big, true)]);
    let url = serve(zip).await;
    let client = reqwest::Client::new();
    let request: RequestFn = Arc::new(move |u: &str| client.get(u));
    let entries = list_remote(&request, &url).await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1].method, 8);

    let dir = std::env::temp_dir().join(format!("dce-zip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let n = extract_remote(&request, &url, &entries[1], &dir.join("big.bin")).await.unwrap();
    assert_eq!(n, big.len() as u64);
    assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), big);
    extract_remote(&request, &url, &entries[0], &dir.join("readme.txt")).await.unwrap();
    assert_eq!(std::fs::read(dir.join("readme.txt")).unwrap(), b"hello zip");
    let _ = std::fs::remove_dir_all(&dir);
}
