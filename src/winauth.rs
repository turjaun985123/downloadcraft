//! Windows logins for company servers: NTLM and Negotiate (Kerberos).
//!
//! NTLM authenticates a connection, not a request, so the usual pooled HTTP
//! client cannot speak it. Windows' own WinHTTP can: it answers the
//! server's challenge with the signed-in user's credentials (intranet sites)
//! or with the username/password of the download. Used only when a server
//! asks for NTLM/Negotiate; one connection, resumable with Range.

use crate::engine::{EngineOpts, SharedState};
use anyhow::{anyhow, Result};
#[cfg(windows)]
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Whether a 401 reply's `WWW-Authenticate` values ask for a Windows login.
pub fn wants_windows_login<'a>(challenges: impl IntoIterator<Item = &'a str>) -> bool {
    challenges.into_iter().any(|c| {
        let scheme = c.trim().split([' ', ',']).next().unwrap_or("").to_ascii_lowercase();
        scheme == "ntlm" || scheme == "negotiate"
    })
}

/// Ask the server (one small request) whether it wants NTLM/Negotiate.
pub async fn server_wants_windows_login(opts: &EngineOpts, url: &str) -> bool {
    let Ok(client) = opts.build_client() else { return false };
    let Ok(resp) = client.get(url).header(reqwest::header::RANGE, "bytes=0-0").send().await else { return false };
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        return false;
    }
    wants_windows_login(resp.headers().get_all(reqwest::header::WWW_AUTHENTICATE).iter().filter_map(|v| v.to_str().ok()))
}

/// Download `url` to `dest` through WinHTTP, continuing a partial file.
pub async fn download(url: &str, dest: &std::path::Path, opts: &EngineOpts, shared: &Arc<SharedState>) -> Result<()> {
    let (url, dest, opts, shared) = (url.to_string(), dest.to_path_buf(), opts.clone(), shared.clone());
    tokio::task::spawn_blocking(move || imp::download(&url, &dest, &opts, &shared))
        .await
        .map_err(|e| anyhow!("windows login download: {e}"))?
}

/// Where the partial file lives while WinHTTP downloads.
fn part_path(dest: &std::path::Path) -> std::path::PathBuf {
    dest.with_extension("dcwin")
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ffi::c_void;
    use std::io::Write;
    use windows_sys::Win32::Networking::WinHttp::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Closes a WinHTTP handle when dropped.
    struct H(*mut c_void);
    impl Drop for H {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    fn last_error(what: &str) -> anyhow::Error {
        anyhow!("{what} failed (Windows error {})", std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }

    unsafe fn query_num(req: *mut c_void, level: u32) -> Option<u64> {
        let mut n: u32 = 0;
        let mut len = 4u32;
        let ok = WinHttpQueryHeaders(req, level | WINHTTP_QUERY_FLAG_NUMBER, std::ptr::null(), &mut n as *mut u32 as *mut c_void, &mut len, std::ptr::null_mut());
        (ok != 0).then_some(n as u64)
    }

    unsafe fn query_text(req: *mut c_void, level: u32) -> Option<String> {
        let mut buf = vec![0u16; 512];
        let mut len = (buf.len() * 2) as u32;
        let ok = WinHttpQueryHeaders(req, level, std::ptr::null(), buf.as_mut_ptr() as *mut c_void, &mut len, std::ptr::null_mut());
        (ok != 0).then(|| String::from_utf16_lossy(&buf[..(len as usize / 2)]))
    }

    pub fn download(url: &str, dest: &std::path::Path, opts: &EngineOpts, shared: &Arc<SharedState>) -> Result<()> {
        let u = reqwest::Url::parse(url).map_err(|_| anyhow!("bad URL: {url}"))?;
        let secure = u.scheme() == "https";
        let host = u.host_str().ok_or_else(|| anyhow!("URL has no host"))?.to_string();
        let port = u.port_or_known_default().unwrap_or(if secure { 443 } else { 80 });
        let object = format!("{}{}", u.path(), u.query().map(|q| format!("?{q}")).unwrap_or_default());
        let part = part_path(dest);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);

        unsafe {
            let agent = wide(if opts.user_agent.is_empty() { "Download Craft" } else { &opts.user_agent });
            let session = H(WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, std::ptr::null(), std::ptr::null(), 0));
            if session.0.is_null() {
                return Err(last_error("WinHttpOpen"));
            }
            let t = (opts.timeout_secs.clamp(5, 300) * 1000) as i32;
            WinHttpSetTimeouts(session.0, t, t, t, t);
            let host_w = wide(&host);
            let conn = H(WinHttpConnect(session.0, host_w.as_ptr(), port, 0));
            if conn.0.is_null() {
                return Err(last_error("WinHttpConnect"));
            }
            let verb = wide("GET");
            let obj = wide(&object);
            let req = H(WinHttpOpenRequest(
                conn.0,
                verb.as_ptr(),
                obj.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                if secure { WINHTTP_FLAG_SECURE } else { 0 },
            ));
            if req.0.is_null() {
                return Err(last_error("WinHttpOpenRequest"));
            }
            let mut headers = String::new();
            if have > 0 {
                headers.push_str(&format!("Range: bytes={have}-\r\n"));
            }
            if !opts.referer.trim().is_empty() {
                headers.push_str(&format!("Referer: {}\r\n", opts.referer.trim()));
            }
            if !opts.cookie.trim().is_empty() {
                headers.push_str(&format!("Cookie: {}\r\n", opts.cookie.trim()));
            }
            if !headers.is_empty() {
                let h = wide(&headers);
                WinHttpAddRequestHeaders(req.0, h.as_ptr(), u32::MAX, WINHTTP_ADDREQ_FLAG_ADD);
            }

            // Send; answer up to three login challenges.
            let user = wide(opts.username.trim());
            let pass = wide(&opts.password);
            let mut status = 0u64;
            for round in 0..4 {
                if WinHttpSendRequest(req.0, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) == 0 {
                    return Err(last_error("sending the request"));
                }
                if WinHttpReceiveResponse(req.0, std::ptr::null_mut()) == 0 {
                    return Err(last_error("reading the reply"));
                }
                status = query_num(req.0, WINHTTP_QUERY_STATUS_CODE).unwrap_or(0);
                if status != 401 || round == 3 {
                    break;
                }
                let (mut supported, mut first, mut target) = (0u32, 0u32, 0u32);
                if WinHttpQueryAuthSchemes(req.0, &mut supported, &mut first, &mut target) == 0 {
                    break;
                }
                let scheme = [WINHTTP_AUTH_SCHEME_NEGOTIATE, WINHTTP_AUTH_SCHEME_NTLM, WINHTTP_AUTH_SCHEME_DIGEST, WINHTTP_AUTH_SCHEME_BASIC]
                    .into_iter()
                    .find(|s| supported & s != 0)
                    .unwrap_or(first);
                let explicit = !opts.username.trim().is_empty();
                // No username: Negotiate/NTLM use the signed-in Windows user
                // (WinHTTP sends it to intranet servers only).
                if !explicit && scheme & (WINHTTP_AUTH_SCHEME_NEGOTIATE | WINHTTP_AUTH_SCHEME_NTLM) == 0 {
                    break;
                }
                let (u_ptr, p_ptr) = if explicit { (user.as_ptr(), pass.as_ptr()) } else { (std::ptr::null(), std::ptr::null()) };
                if WinHttpSetCredentials(req.0, target, scheme, u_ptr, p_ptr, std::ptr::null_mut()) == 0 {
                    return Err(last_error("setting the login"));
                }
            }
            match status {
                200 => {
                    // The server ignored the Range: start the file over.
                    have = 0;
                }
                206 => {}
                401 => return Err(anyhow!("the server refused the Windows login (401) — check the username (DOMAIN\\user) and password")),
                s => return Err(anyhow!("HTTP {s}")),
            }
            let rest = query_num(req.0, WINHTTP_QUERY_CONTENT_LENGTH);
            let total = match (status, query_text(req.0, WINHTTP_QUERY_CONTENT_RANGE)) {
                (206, Some(cr)) => cr.rsplit('/').next().and_then(|t| t.trim().parse().ok()),
                _ => rest,
            };
            if let Some(t) = total {
                shared.total.store(t, Ordering::SeqCst);
                shared.has_total.store(true, Ordering::SeqCst);
            }
            shared.resumable.store(status == 206, Ordering::SeqCst);
            let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(have == 0).append(have > 0).open(&part)?;
            shared.downloaded.store(have, Ordering::SeqCst);
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                if shared.cancelled.load(Ordering::SeqCst) {
                    file.flush()?;
                    return Err(anyhow!("cancelled"));
                }
                while shared.paused.load(Ordering::SeqCst) && !shared.cancelled.load(Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                let mut n = 0u32;
                if WinHttpReadData(req.0, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut n) == 0 {
                    file.flush()?;
                    return Err(last_error("reading data"));
                }
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n as usize])?;
                have += n as u64;
                shared.downloaded.store(have, Ordering::SeqCst);
            }
            file.flush()?;
            drop(file);
            if let Some(t) = total {
                if have < t {
                    return Err(anyhow!("connection ended early (got {have} of {t} bytes) — resuming"));
                }
            }
        }
        if dest.exists() {
            std::fs::remove_file(dest)?;
        }
        std::fs::rename(&part, dest)?;
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub fn download(_url: &str, _dest: &std::path::Path, _opts: &EngineOpts, _shared: &Arc<SharedState>) -> Result<()> {
        Err(anyhow!("NTLM/Negotiate logins need Windows"))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn windows_login_challenges_are_recognised() {
        assert!(wants_windows_login(["NTLM"]));
        assert!(wants_windows_login(["Basic realm=\"x\"", "Negotiate"]));
        assert!(wants_windows_login(["negotiate abc=="]));
        assert!(!wants_windows_login(["Basic realm=\"x\"", "Digest realm=\"y\", nonce=\"1\""]));
    }

    /// A server that wants a login (Basic, which WinHTTP answers through the
    /// same challenge loop as NTLM) and serves ranges.
    async fn serve(body: Vec<u8>) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}/f.bin", l.local_addr().unwrap().port());
        let body = Arc::new(body);
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { break };
                let body = body.clone();
                tokio::spawn(async move {
                    loop {
                        let mut head = Vec::new();
                        let mut b = [0u8; 1];
                        while !head.ends_with(b"\r\n\r\n") {
                            if s.read(&mut b).await.unwrap_or(0) == 0 {
                                return;
                            }
                            head.push(b[0]);
                        }
                        let text = String::from_utf8_lossy(&head).to_lowercase();
                        // "u:p" in base64
                        if !text.contains("authorization: basic dtpw") {
                            let _ = s.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"t\"\r\nContent-Length: 0\r\n\r\n").await;
                            continue;
                        }
                        let from: usize = text
                            .lines()
                            .find_map(|l| l.strip_prefix("range: bytes="))
                            .and_then(|r| r.split('-').next())
                            .and_then(|x| x.trim().parse().ok())
                            .unwrap_or(0);
                        let part = &body[from.min(body.len())..];
                        let head = if from > 0 {
                            format!("HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {from}-{}/{}\r\n\r\n", part.len(), body.len() - 1, body.len())
                        } else {
                            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", part.len())
                        };
                        let _ = s.write_all(head.as_bytes()).await;
                        let _ = s.write_all(part).await;
                    }
                });
            }
        });
        base
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn winhttp_logs_in_and_resumes() {
        let body: Vec<u8> = (0..400_000u32).map(|i| (i % 249) as u8).collect();
        let url = serve(body.clone()).await;
        let dir = std::env::temp_dir().join(format!("dc_winauth_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let opts = EngineOpts { username: "u".into(), password: "p".into(), timeout_secs: 10, ..Default::default() };
        let dest = dir.join("full.bin");
        download(&url, &dest, &opts, &Arc::new(SharedState::new())).await.expect("download");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        // A partial file continues where it stopped.
        let dest = dir.join("resume.bin");
        std::fs::write(part_path(&dest), &body[..123_456]).unwrap();
        let shared = Arc::new(SharedState::new());
        download(&url, &dest, &opts, &shared).await.expect("resume");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(shared.resumable.load(Ordering::SeqCst));
        // Wrong password: a clear refusal.
        let bad = EngineOpts { password: "nope".into(), ..opts };
        let err = download(&url, &dir.join("bad.bin"), &bad, &Arc::new(SharedState::new())).await.expect_err("refused");
        assert!(format!("{err:#}").contains("401"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
