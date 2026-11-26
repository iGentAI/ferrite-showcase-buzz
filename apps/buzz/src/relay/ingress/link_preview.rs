//! Link previews: a bounded fetch of the page and the title and icon read from it.

use ferrite_app::tracing;
use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use ferrite_app::AppContext;
use ferrite_app::IngressRequest;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use crate::relay::DEFAULT_COMMUNITY;
use super::{authenticate_request, enforce_write_gate, storage};

/// How much of a fetched document is scanned for a `<title>`.
///
/// A PARSE bound, not a memory bound, and the difference matters: the egress API
/// hands back a fully-buffered body, so by the time this slice is taken the bytes
/// are already held. What bounds memory is the platform's own egress reply cap,
/// which is where a bound of that kind belongs — the app never sees the socket and
/// cannot stop reading early. This only stops the app SCANNING a large document,
/// which is worth doing because a `<title>` lives in the head.
pub(super) const LINK_PREVIEW_MAX_BYTES: usize = 64 * 1024;

/// `POST /buzz/api/link-preview` — the title behind a URL.
///
/// The desktop app fetches this through Rust, which has no same-origin policy. A
/// browser tab does, so this was refused as CORS-blocked — true of the tab, and
/// beside the point: the RELAY has egress, so the fetch belongs here.
///
/// What the app adds on top of the platform's destination scoping, none of it
/// optional:
///
/// * **https only**, so a preview cannot be used to probe plaintext services;
/// * **no userinfo in the authority**, so a pasted `https://u:p@host/` cannot
///   send a credential somewhere on the relay's behalf;
/// * **only the title is returned** — no body, no headers, no status detail, so a
///   preview cannot become a general-purpose proxy for reading internal pages;
/// * **a parse bound** on how much of the document is scanned (see
///   [`LINK_PREVIEW_MAX_BYTES`], which is deliberately NOT a memory bound).
///
/// **Which hosts can be previewed is the DEPLOYMENT's declaration, not this
/// route's.** The platform refuses an egress authority that names no host
/// (`APP_EGRESS_DESTINATION_MALFORMED` — a wildcard `https://` is rejected at
/// validation), because destination scoping IS the capability. So the spec lists
/// the hosts this deployment may fetch, exactly as it lists the model providers,
/// and an operator who wants their team's wiki previewed adds it there.
///
/// A URL outside that scope is answered `title: null` rather than an error: the
/// platform denies it, the client renders the bare link, and that is the same
/// thing it does for a page that simply has no title. A preview is decoration,
/// and decoration must not become a failure.
///
/// Private-range destinations and the reply size are refused and bounded by the
/// platform's own egress front, which is where both belong: an app-level IP
/// allowlist would be a second opinion about a network the app cannot see, and
/// the app never holds the socket to stop reading on.
pub(super) async fn link_preview(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    // A fetch on the relay's behalf is a write-shaped act: it spends the
    // deployment's egress budget and reaches the network as the relay. So it is
    // gated and limited like one.
    enforce_write_gate(ctx, community, &caller.pubkey, now_ms).await?;
    match store::bump_expiring_counter(
        &mut config,
        &keys::rate(community, &caller.pubkey, "preview"),
        1,
        now_ms.saturating_add(60_000),
    )
    .await
    {
        Ok(count) if count > 60 => return Err(BuzzError::RateLimited),
        Ok(_) => {}
        Err(e) => tracing::warn!("link preview limiter failed open: {e}"),
    }

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("preview body: {e}")))?;
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if !url.starts_with("https://") || url.len() > 2048 {
        return Err(BuzzError::InvalidEvent(
            "a link preview needs an https URL of at most 2048 bytes".into(),
        ));
    }
    // Printable, single-token ASCII: a URL carrying whitespace or control bytes is
    // not a URL a client pasted, it is something being smuggled.
    if !url.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(BuzzError::InvalidEvent(
            "that URL carries whitespace or control bytes".into(),
        ));
    }
    if crate::relay::operator::authority_has_userinfo(url) {
        return Err(BuzzError::InvalidEvent(
            "a link preview URL must not carry userinfo (user:pass@host)".into(),
        ));
    }

    // No egress binding ⇒ this deployment does not do previews. An empty title is
    // the honest answer: the client renders the bare link, which is what it does
    // for a page with no title anyway.
    let Ok(mut egress) = ctx.egress(store::LINK_PREVIEW_EGRESS) else {
        return serde_json::to_vec(&serde_json::json!({ "ok": true, "url": url, "title": Value::Null }))
            .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")));
    };
    let outcome = egress
        .deliver_http(ferrite_app::EgressHttpMethod::Get, url, &[], &[])
        .await
        .map_err(|e| BuzzError::Storage(format!("link preview fetch: {e}")))?;
    let mut title = None;
    let mut icon = None;
    if let ferrite_app::EgressOutcome::Delivered { status, body, .. } = outcome {
        if (200..300).contains(&status) {
            let head = body.get(..LINK_PREVIEW_MAX_BYTES).unwrap_or(&body);
            let text = String::from_utf8_lossy(head);
            title = html_title(&text);
            icon = html_icon(&text, url);
        }
    }
    // A refusal is not an error the caller can act on: the platform declined the
    // destination, or the page did not answer. Either way there is no preview,
    // which is what the client renders around.
    serde_json::to_vec(&serde_json::json!({ "ok": true, "url": url, "title": title, "icon": icon }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// The page's icon, as an ABSOLUTE url.
///
/// Resolved against the document rather than returned raw, because a relative
/// `href="/favicon.ico"` means nothing to a caller that never saw the page. Only
/// the URL is returned — never bytes — so this stays a metadata route rather
/// than becoming a proxy for arbitrary content.
pub(super) fn html_icon(html: &str, page_url: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let mut at = 0usize;
    while let Some(found) = lower.get(at..)?.find("<link") {
        let start = at + found;
        let end = lower.get(start..)?.find('>').map(|e| start + e)?;
        let tag = lower.get(start..end)?;
        at = end + 1;
        // `rel="icon"`, `shortcut icon`, `apple-touch-icon` — matched as a
        // substring of the rel value, which is what makes the multi-token form
        // (`rel="shortcut icon"`) resolve.
        if !tag_attr(tag, "rel").is_some_and(|rel| rel.contains("icon")) {
            continue;
        }
        // The href comes from the ORIGINAL case, since a URL path is
        // case-sensitive and the lowercased copy exists only for matching.
        let Some(raw_tag) = html.get(start..end) else {
            continue;
        };
        if let Some(href) = tag_attr(raw_tag, "href") {
            if let Some(absolute) = absolutize(&href, page_url) {
                return Some(absolute);
            }
        }
    }
    None
}

/// One quoted attribute value out of a tag, or `None`.
pub(super) fn tag_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let at = lower.find(&format!("{name}="))?;
    let after = tag.get(at + name.len() + 1..)?;
    let quote = after.chars().next()?;
    if quote != '"' && quote != '\'' {
        // Unquoted: value runs to the next whitespace.
        let value = after.split_whitespace().next()?;
        return (!value.is_empty()).then(|| value.to_string());
    }
    let body = after.get(1..)?;
    let end = body.find(quote)?;
    let value = body.get(..end)?;
    (!value.is_empty()).then(|| value.to_string())
}

/// Resolve a possibly-relative URL against the page it came from.
///
/// Only https survives, for the same reason the request itself is https-only:
/// a page must not be able to hand a client back a plaintext or `javascript:`
/// URL that the client then loads.
pub(super) fn absolutize(href: &str, page_url: &str) -> Option<String> {
    let href = href.trim();
    if href.is_empty() || href.len() > 2048 {
        return None;
    }
    if href.starts_with("https://") {
        return Some(href.to_string());
    }
    // A scheme we do not accept (http, data, javascript, …) is refused rather
    // than rewritten — rewriting would invent an address the page never gave.
    if href.contains("://") || href.starts_with("//") || href.starts_with("data:") {
        return None;
    }
    let origin_end = page_url
        .get("https://".len()..)?
        .find('/')
        .map_or(page_url.len(), |slash| "https://".len() + slash);
    let origin = page_url.get(..origin_end)?;
    if href.starts_with('/') {
        return Some(format!("{origin}{href}"));
    }
    // Document-relative: resolved against the page's directory.
    let dir_end = page_url.rfind('/').filter(|at| *at >= origin_end)?;
    let dir = page_url.get(..dir_end)?;
    Some(format!("{dir}/{href}"))
}

/// The text of the first `<title>` element, trimmed and bounded.
///
/// A deliberately small parser rather than a dependency: the app has one, and a
/// title is a single element with no nesting to track. Entity decoding is limited
/// to the five that actually appear in titles — anything else is left literal,
/// which is a display nuisance and never an injection, because this string is
/// returned as JSON and rendered as text.
pub(super) fn html_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let open = lower.find("<title")?;
    let after = lower.get(open..)?.find('>')? + open + 1;
    let close = lower.get(after..)?.find("</title>")? + after;
    let raw = html.get(after..close)?;
    let decoded = raw
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let title: String = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then(|| title.chars().take(300).collect())
}
