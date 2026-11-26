use ferrite_app::serde_json::{self, Value};

pub(super) const WEB_BRAVE_KEY: &[u8] = b"brave-search-api-key";
pub(super) const WEB_BRAVE_WEB_URL: &[u8] = b"brave-web-url";
pub(super) const WEB_BRAVE_NEWS_URL: &[u8] = b"brave-news-url";
pub(super) const WEB_PERPLEXITY_KEY: &[u8] = b"perplexity-api-key";
pub(super) const WEB_PERPLEXITY_CHAT_URL: &[u8] = b"perplexity-chat-url";
pub(super) const WEB_PERPLEXITY_SEARCH_URL: &[u8] = b"perplexity-search-url";
pub(super) const WEB_CLOUDFLARE_ACCOUNT_ID: &[u8] = b"cloudflare-account-id";
pub(super) const WEB_CLOUDFLARE_BROWSER_TOKEN: &[u8] = b"cloudflare-browser-token";
pub(super) const WEB_CLOUDFLARE_API_BASE: &[u8] = b"cloudflare-api-base";
pub(super) const DEFAULT_BRAVE_WEB_URL: &str = "https://api.search.brave.com/res/v1/web/search";
pub(super) const DEFAULT_BRAVE_NEWS_URL: &str = "https://api.search.brave.com/res/v1/news/search";
pub(super) const DEFAULT_PERPLEXITY_CHAT_URL: &str = "https://api.perplexity.ai/chat/completions";
pub(super) const DEFAULT_PERPLEXITY_SEARCH_URL: &str = "https://api.perplexity.ai/search";
pub(super) const DEFAULT_CLOUDFLARE_API_BASE: &str = "https://api.cloudflare.com/client/v4";

#[derive(Clone)]
pub(super) struct WebBinding {
    pub(super) egress_handle: String,
    pub(super) config_store_handle: String,
}

#[derive(Debug, Default)]
pub(super) struct WebConfigDoc {
    pub(super) brave_key: Option<String>,
    pub(super) brave_web_url: String,
    pub(super) brave_news_url: String,
    pub(super) perplexity_key: Option<String>,
    pub(super) perplexity_chat_url: String,
    pub(super) perplexity_search_url: String,
    pub(super) cloudflare_account_id: Option<String>,
    pub(super) cloudflare_browser_token: Option<String>,
    pub(super) cloudflare_api_base: String,
}

pub(super) fn cloudflare_url(
    cfg: &WebConfigDoc,
    endpoint: &str,
) -> std::result::Result<String, String> {
    let account = cfg.cloudflare_account_id.as_deref().ok_or_else(|| {
        format!(
            "missing credential: {}",
            String::from_utf8_lossy(WEB_CLOUDFLARE_ACCOUNT_ID)
        )
    })?;
    Ok(format!(
        "{}/accounts/{}/browser-rendering/{}",
        cfg.cloudflare_api_base.trim_end_matches('/'),
        encode_path_segment(account)?,
        encode_path_segment(endpoint)?
    ))
}

/// Longest crawl job id accepted. Cloudflare issues UUIDs (36 characters).
const MAX_CRAWL_JOB_ID_LEN: usize = 128;

/// The URL for polling one crawl job. The job id comes from the model and the
/// request carries the host's Cloudflare token, so the id must stay one opaque
/// segment under `browser-rendering/crawl/`: only ASCII letters, digits, `-`
/// and `_` are accepted, the same rule `ferrite-web` applies.
pub(super) fn cloudflare_crawl_status_url(
    cfg: &WebConfigDoc,
    job_id: &str,
) -> std::result::Result<String, String> {
    let well_formed = !job_id.is_empty()
        && job_id.len() <= MAX_CRAWL_JOB_ID_LEN
        && job_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !well_formed {
        return Err(format!(
            "crawl job_id must be 1-{MAX_CRAWL_JOB_ID_LEN} ASCII letters, digits, '-' or '_'"
        ));
    }
    Ok(format!("{}/{job_id}", cloudflare_url(cfg, "crawl")?))
}

pub(super) fn cloudflare_headers(
    cfg: &WebConfigDoc,
) -> std::result::Result<Vec<(String, String)>, String> {
    let token = cfg.cloudflare_browser_token.as_deref().ok_or_else(|| {
        format!(
            "missing credential: {}",
            String::from_utf8_lossy(WEB_CLOUDFLARE_BROWSER_TOKEN)
        )
    })?;
    Ok(vec![
        ("authorization".to_string(), format!("Bearer {token}")),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ])
}

pub(super) fn unwrap_cloudflare_envelope(value: Value) -> std::result::Result<Value, String> {
    if value.get("success").and_then(Value::as_bool) == Some(true) {
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    } else {
        Err(format!(
            "Cloudflare error: {}",
            value.get("errors").cloned().unwrap_or(Value::Null)
        ))
    }
}

pub(super) fn value_field(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or(Value::Null)
}

pub(super) fn trim_brave_web_result(result: &Value) -> Value {
    serde_json::json!({
        "title": value_field(result, "title"),
        "url": value_field(result, "url"),
        "description": value_field(result, "description"),
        "age": value_field(result, "age"),
        "page_age": value_field(result, "page_age"),
    })
}

pub(super) fn trim_brave_news_result(result: &Value) -> Value {
    serde_json::json!({
        "title": value_field(result, "title"),
        "url": value_field(result, "url"),
        "description": value_field(result, "description"),
        "age": value_field(result, "age"),
        "breaking": value_field(result, "breaking"),
    })
}

pub(super) fn trim_perplexity_result(result: &Value) -> Value {
    serde_json::json!({
        "title": value_field(result, "title"),
        "url": value_field(result, "url"),
        "snippet": value_field(result, "snippet"),
        "date": value_field(result, "date"),
        "last_updated": value_field(result, "last_updated"),
    })
}

pub(super) fn binary_json(content_type: &str, bytes: &[u8]) -> Value {
    serde_json::json!({
        "content_type": content_type,
        "bytes": bytes.len(),
        "base64": base64_encode(bytes),
    })
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

pub(super) fn with_query(base: &str, params: &[(String, String)]) -> String {
    let mut out = base.to_string();
    out.push(if base.contains('?') { '&' } else { '?' });
    for (idx, (key, value)) in params.iter().enumerate() {
        if idx > 0 {
            out.push('&');
        }
        out.push_str(&encode_query_component(key));
        out.push('=');
        out.push_str(&encode_query_component(value));
    }
    out
}

/// Percent-encode one path segment. An empty, `.` or `..` segment is
/// refused: percent-encoding leaves dots alone, and a URL parser resolves
/// `%2e` as a dot anyway, so such a segment would walk the path instead of
/// naming something inside it.
pub(super) fn encode_path_segment(value: &str) -> std::result::Result<String, String> {
    if matches!(value, "" | "." | "..") {
        return Err(format!("path segment {value:?} is not allowed"));
    }
    Ok(percent_encode(value, false))
}

fn encode_query_component(value: &str) -> String {
    percent_encode(value, true)
}

fn percent_encode(value: &str, space_as_plus: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::new();
    for byte in value.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if keep {
            out.push(byte as char);
        } else if byte == b' ' && space_as_plus {
            out.push('+');
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}
