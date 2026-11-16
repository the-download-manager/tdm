//! HTTP client: capability probing, per-connection range requests, header handling.

use crate::config::Config;
use crate::model::Download;
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, COOKIE, ETAG,
    LAST_MODIFIED, RANGE, REFERER, USER_AGENT,
};
use reqwest::{Client, Response};
use std::time::Duration;
use url::Url;

#[derive(Debug, Clone, Default)]
pub struct RemoteInfo {
    pub size: u64,
    pub ranges: bool,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub final_url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub struct HttpClient {
    pub client: Client,
    pub default_ua: String,
}

impl HttpClient {
    pub fn new(cfg: &Config) -> Result<Self> {
        let mut builder = Client::builder()
            .user_agent(&cfg.user_agent)
            .cookie_store(true)
            .redirect(reqwest::redirect::Policy::limited(10))
            .connect_timeout(Duration::from_secs(20))
            // ranged downloads need one socket per worker, so don't queue them on a pool
            .pool_max_idle_per_host(0);
        if let Some(proxy) = &cfg.proxy {
            builder = builder.proxy(reqwest::Proxy::all(proxy).context("invalid proxy URL")?);
        }
        Ok(Self {
            client: builder.build()?,
            default_ua: cfg.user_agent.clone(),
        })
    }

    fn apply_headers(&self, mut req: reqwest::RequestBuilder, dl: &Download) -> reqwest::RequestBuilder {
        req = req.header(ACCEPT, "*/*");
        // Never let the transport decompress: byte offsets would no longer match the file.
        req = req.header(ACCEPT_ENCODING, "identity");
        let ua = dl.user_agent.as_deref().unwrap_or(&self.default_ua);
        req = req.header(USER_AGENT, ua);
        if let Some(referer) = &dl.referer {
            req = req.header(REFERER, referer);
        }
        if let Some(cookie) = &dl.cookie {
            req = req.header(COOKIE, cookie);
        }
        for header in &dl.headers {
            req = req.header(header.name.as_str(), header.value.as_str());
        }
        req
    }

    /// Ask the server for the first byte only: this reveals size, range support,
    /// the real file name and the validators we need for safe resume.
    pub async fn probe(&self, dl: &Download) -> Result<RemoteInfo> {
        let (url, auth) = split_auth(&dl.url)?;
        let mut req = self.apply_headers(self.client.get(url.clone()), dl);
        req = req.header(RANGE, "bytes=0-0");
        if let Some((user, pass)) = auth {
            req = req.basic_auth(user, pass);
        }

        let resp = req.send().await.context("connecting to server")?;
        let status = resp.status();
        let final_url = resp.url().to_string();
        let headers = resp.headers().clone();

        let mut info = RemoteInfo {
            final_url: final_url.clone(),
            content_type: headers
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string()),
            etag: headers.get(ETAG).and_then(|v| v.to_str().ok()).map(|s| s.to_string()),
            last_modified: headers
                .get(LAST_MODIFIED)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string()),
            ..Default::default()
        };

        if let Some(cd) = headers.get(CONTENT_DISPOSITION).and_then(|v| v.to_str().ok()) {
            info.filename = filename_from_disposition(cd);
        }

        if status == reqwest::StatusCode::PARTIAL_CONTENT {
            info.ranges = true;
            if let Some(cr) = headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
                info.size = parse_content_range_total(cr).unwrap_or(0);
            }
        } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // e.g. "bytes */1234" - file exists but the range was not usable
            info.ranges = true;
            if let Some(cr) = headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
                info.size = parse_content_range_total(cr).unwrap_or(0);
            }
        } else if status.is_success() {
            info.ranges = false;
            info.size = headers
                .get(CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
        } else if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            bail!("server returned {} (authentication required?)", status);
        } else {
            bail!("server returned HTTP {}", status);
        }

        if info.filename.is_none() {
            info.filename = filename_from_url(&final_url);
        }
        Ok(info)
    }

    pub async fn get_range(&self, dl: &Download, start: u64, end_inclusive: u64) -> Result<Response> {
        let (url, auth) = split_auth(dl.final_url.as_deref().unwrap_or(&dl.url))?;
        let mut req = self
            .client
            .get(url)
            .header(RANGE, format!("bytes={}-{}", start, end_inclusive));
        req = self.apply_headers(req, dl);
        if let Some((user, pass)) = auth {
            req = req.basic_auth(user, pass);
        }
        let resp = req.send().await.context("range request failed")?;
        match resp.status() {
            reqwest::StatusCode::PARTIAL_CONTENT => Ok(resp),
            reqwest::StatusCode::OK => Err(anyhow!("server ignored Range header (HTTP 200)")),
            other => bail!("range request returned HTTP {}", other),
        }
    }

    /// Plain streaming GET for servers without range support.
    pub async fn get_full(&self, dl: &Download) -> Result<Response> {
        let (url, auth) = split_auth(dl.final_url.as_deref().unwrap_or(&dl.url))?;
        let mut req = self.client.get(url);
        req = self.apply_headers(req, dl);
        if let Some((user, pass)) = auth {
            req = req.basic_auth(user, pass);
        }
        let resp = req.send().await.context("request failed")?;
        if !resp.status().is_success() {
            bail!("server returned HTTP {}", resp.status());
        }
        Ok(resp)
    }

    /// Small helper for manifests/segments: GET with the default UA and optional referer.
    async fn plain_get(&self, url: &str, referer: Option<&str>) -> Result<Response> {
        let mut req = self
            .client
            .get(url)
            .header(ACCEPT, "*/*")
            .header(ACCEPT_ENCODING, "identity")
            .header(USER_AGENT, &self.default_ua);
        if let Some(referer) = referer {
            req = req.header(REFERER, referer);
        }
        let resp = req.send().await.with_context(|| format!("GET {}", url))?;
        if !resp.status().is_success() {
            bail!("GET {} returned HTTP {}", url, resp.status());
        }
        Ok(resp)
    }

    pub async fn fetch_bytes(&self, url: &str, referer: Option<&str>) -> Result<Vec<u8>> {
        let resp = self.plain_get(url, referer).await?;
        Ok(resp.bytes().await?.to_vec())
    }

    pub async fn fetch_text(&self, url: &str, referer: Option<&str>) -> Result<String> {
        let bytes = self.fetch_bytes(url, referer).await?;
        Ok(String::from_utf8_lossy(&bytes).to_string())
    }

    /// Stream a URL straight to a file (used for captured video/audio streams).
    pub async fn download_file(
        &self,
        url: &str,
        path: &std::path::Path,
        referer: Option<&str>,
        cookie: Option<&str>,
        user_agent: Option<&str>,
    ) -> Result<u64> {
        let mut req = self
            .client
            .get(url)
            .header(ACCEPT, "*/*")
            .header(ACCEPT_ENCODING, "identity")
            .header(USER_AGENT, user_agent.unwrap_or(&self.default_ua));
        if let Some(referer) = referer {
            req = req.header(REFERER, referer);
        }
        if let Some(cookie) = cookie {
            req = req.header(COOKIE, cookie);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            bail!("GET {} returned HTTP {}", url, resp.status());
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut file = tokio::fs::File::create(path).await?;
        let mut stream = resp.bytes_stream();
        let mut total = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
            total += chunk.len() as u64;
        }
        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        Ok(total)
    }

    /// Range GET used for HLS byte-range segments.
    pub async fn fetch_range(
        &self,
        url: &str,
        start: u64,
        len: u64,
        referer: Option<&str>,
    ) -> Result<Vec<u8>> {
        let mut req = self
            .client
            .get(url)
            .header(ACCEPT, "*/*")
            .header(ACCEPT_ENCODING, "identity")
            .header(USER_AGENT, &self.default_ua)
            .header(RANGE, format!("bytes={}-{}", start, start + len.saturating_sub(1)));
        if let Some(referer) = referer {
            req = req.header(REFERER, referer);
        }
        let resp = req.send().await?;
        if !(resp.status().is_success() || resp.status() == reqwest::StatusCode::PARTIAL_CONTENT) {
            bail!("range GET {} returned HTTP {}", url, resp.status());
        }
        Ok(resp.bytes().await?.to_vec())
    }
}

/// Split `scheme://user:pass@host/...` into a credential-free URL plus basic-auth pair.
pub fn split_auth(raw: &str) -> Result<(Url, Option<(String, Option<String>)>)> {
    let mut url = Url::parse(raw).with_context(|| format!("invalid URL: {}", raw))?;
    let user = url.username().to_string();
    if user.is_empty() {
        return Ok((url, None));
    }
    let pass = url.password().map(|p| p.to_string());
    url.set_username("").ok();
    url.set_password(None).ok();
    Ok((url, Some((percent_decode(&user), pass.map(|p| percent_decode(&p))))))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `bytes 0-0/12345` or `bytes */12345` -> 12345
pub fn parse_content_range_total(value: &str) -> Option<u64> {
    let (_, total) = value.rsplit_once('/')?;
    total.trim().parse().ok()
}

pub fn filename_from_disposition(value: &str) -> Option<String> {
    // RFC 5987: filename*=UTF-8''na%C3%AFve%20file.txt
    for part in value.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename*=") {
            let rest = rest.trim_matches('"');
            let name = rest.splitn(3, '\'').nth(2).unwrap_or(rest);
            let decoded = percent_decode(name);
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    for part in value.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename=") {
            let name = rest.trim().trim_matches('"');
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

pub fn filename_from_url(raw: &str) -> Option<String> {
    let url = Url::parse(raw).ok()?;
    let last = url.path_segments()?.last()?.to_string();
    if last.is_empty() {
        return None;
    }
    Some(percent_decode(&last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes */99"), Some(99));
        assert_eq!(parse_content_range_total("garbage"), None);
    }

    #[test]
    fn disposition_prefers_rfc5987() {
        assert_eq!(
            filename_from_disposition("attachment; filename=\"fallback.bin\"; filename*=UTF-8''na%C3%AFve.bin"),
            Some("naïve.bin".to_string())
        );
        assert_eq!(
            filename_from_disposition("attachment; filename=\"plain.zip\""),
            Some("plain.zip".to_string())
        );
    }

    #[test]
    fn auth_is_stripped_from_url() {
        let (url, auth) = split_auth("https://bob:s%40cret@example.com/f.zip").unwrap();
        assert_eq!(url.host_str(), Some("example.com"));
        assert!(!url.as_str().contains("bob"));
        let (user, pass) = auth.unwrap();
        assert_eq!(user, "bob");
        assert_eq!(pass.as_deref(), Some("s@cret"));
    }

    #[test]
    fn filename_from_url_extracts_last_segment() {
        assert_eq!(
            filename_from_url("https://example.com/a/b/file%20name.zip?x=1"),
            Some("file name.zip".to_string())
        );
    }
}
