//! Serving the web client's assets from the blob plane.

use ferrite_app::tracing;
use ferrite_app::AppContext;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::store;
use super::{HEADER_CONTENT_TYPE, Reply};
use super::operator_plane::{current_ui_manifest};

/// Content-addressed blob key for a committed UI bundle asset (`ui-sha256/{sha256}`).
///
/// Immutable across bundle replacements and retained when unversioned `ui/` keys
/// are pruned, so an in-flight `/buzz/ui/` read that selected an older manifest
/// still resolves that manifest's exact file bytes without mixing versions or
/// hitting a transient 404.
pub(super) fn ui_asset_versioned_key(sha256: &str) -> String {
    format!("ui-sha256/{sha256}")
}

/// One file of the web client for `/buzz/ui/{rest}`.
///
/// The bundle an operator pushed through the assets routes is the authority once its manifest is committed
/// (`operator::MANIFEST_CONFIG`): its files are resolved through the committed manifest's content digest from
/// the `web-assets` blob (`ui-sha256/{sha256}`, falling back to `ui/{path}` only when its digest matches), and
/// a path the manifest does not name is not served from the embedded table, so the two bundles are never mixed.
/// Until a bundle has been pushed (or when the manifest row is unreadable, which the push routes also treat as
/// absent) the bundle embedded in this build answers. A storage fault while a pushed bundle may exist answers
/// 503 rather than the embedded bundle the push replaced.
///
/// A path with no file extension is a client route and gets `index.html`; hashed files under `assets/` never
/// change and are cached for a year.
pub(super) async fn ui_asset(ctx: &AppContext, path: &str, head: bool) -> Reply {
    let rest = path.trim_start_matches("/buzz/ui/");
    let unavailable = |what: &str, e: &dyn std::fmt::Display| {
        tracing::warn!("web client {what} unavailable: {e}");
        Reply::bodyless(503).with_header("retry-after", "1")
    };
    let manifest = match planes::store(ctx, store::CONFIG) {
        Ok(mut config) => match current_ui_manifest(&mut config).await {
            Ok(manifest) => manifest,
            Err(e) => return unavailable("manifest", &e),
        },
        Err(e) => return unavailable("config binding", &e),
    };
    let Some(manifest) = manifest else {
        let find = |name: &str| {
            crate::relay::assets::ui::UI_ASSETS
                .iter()
                .find(|(key, _)| key.strip_prefix("ui/") == Some(name))
                .map(|(_, bytes)| *bytes)
        };
        return match ui_asset_name(rest, |name| find(name).is_some()) {
            Some(name) => match find(name) {
                Some(bytes) => ui_asset_reply(name, bytes, head, None),
                None => Reply::bodyless(404),
            },
            None => Reply::bodyless(404),
        };
    };
    let Some(name) = ui_asset_name(rest, |name| manifest.files.iter().any(|f| f.path == name))
    else {
        return Reply::bodyless(404);
    };
    resolve_ui_manifest_reply(ctx, &manifest, name, head, || async {}).await
}

/// Resolve one `name` of a committed `manifest` from `web-assets`, running
/// `after_first_versioned_miss` if the initial content-addressed lookup misses
/// before consulting the unversioned `ui/` fallback and final content-addressed
/// retry, and return `(status, body)`.
#[doc(hidden)]
pub async fn resolve_ui_manifest_file<F, Fut>(
    ctx: &AppContext,
    manifest: &crate::relay::operator::Manifest,
    name: &str,
    head: bool,
    after_first_versioned_miss: F,
) -> (u16, Vec<u8>)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let reply =
        resolve_ui_manifest_reply(ctx, manifest, name, head, after_first_versioned_miss).await;
    (reply.status, reply.body)
}

pub(super) async fn resolve_ui_manifest_reply<F, Fut>(
    ctx: &AppContext,
    manifest: &crate::relay::operator::Manifest,
    name: &str,
    head: bool,
    after_first_versioned_miss: F,
) -> Reply
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let unavailable = |what: &str, e: &dyn std::fmt::Display| {
        tracing::warn!("web client {what} unavailable: {e}");
        Reply::bodyless(503).with_header("retry-after", "1")
    };
    let Some(file) = manifest.files.iter().find(|f| f.path == name) else {
        return Reply::bodyless(404);
    };
    let Some(unversioned_key) = keys::ui_asset(name) else {
        return Reply::bodyless(404);
    };
    let versioned_key = ui_asset_versioned_key(&file.sha256);
    let mut assets = match planes::blob(ctx, store::WEB_ASSETS) {
        Ok(assets) => assets,
        Err(e) => return unavailable("web-assets binding", &e),
    };
    match assets.get(versioned_key.as_bytes()).await {
        Ok(Some(bytes)) => return ui_asset_reply(name, &bytes, head, Some(&file.sha256)),
        Ok(None) => {}
        Err(e) => return unavailable("asset", &e),
    }
    after_first_versioned_miss().await;
    match assets.get(unversioned_key.as_bytes()).await {
        Ok(Some(bytes))
            if u64::try_from(bytes.len()).ok() == Some(file.size)
                && crate::relay::crypto::to_hex(&crate::relay::crypto::sha256::sha256(&bytes))
                    == file.sha256 =>
        {
            return ui_asset_reply(name, &bytes, head, Some(&file.sha256));
        }
        Ok(Some(_)) | Ok(None) => {}
        Err(e) => return unavailable("asset", &e),
    }
    match assets.get(versioned_key.as_bytes()).await {
        Ok(Some(bytes)) => ui_asset_reply(name, &bytes, head, Some(&file.sha256)),
        Ok(None) => {
            tracing::warn!(
                "bundle {} names {name} ({}), which the web-assets blob does not hold",
                manifest.etag,
                file.sha256
            );
            Reply::bodyless(404)
        }
        Err(e) => unavailable("asset", &e),
    }
}

/// Which bundle file answers `/buzz/ui/{rest}`: the file itself when the bundle has it, `index.html` for the
/// bare prefix and for a client route (last segment without an extension), otherwise none.
pub(super) fn ui_asset_name(rest: &str, has: impl Fn(&str) -> bool) -> Option<&str> {
    if !rest.is_empty() && has(rest) {
        return Some(rest);
    }
    let last = rest.rsplit('/').next().unwrap_or("");
    ((rest.is_empty() || !last.contains('.')) && has("index.html")).then_some("index.html")
}

pub(super) fn ui_asset_reply(name: &str, bytes: &[u8], head: bool, pushed_sha256: Option<&str>) -> Reply {
    let mime = match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "json" | "webmanifest" | "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    };
    // Operator-pushed bundle paths need not be content-hashed (an operator may
    // replace bytes at the same path across pushes), so only the build-time
    // embedded hashed assets get a year-long immutable cache header.
    let cache = if pushed_sha256.is_none() && name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let reply = Reply {
        status: 200,
        headers: vec![(HEADER_CONTENT_TYPE.to_string(), mime.to_string())],
        body: if head { Vec::new() } else { bytes.to_vec() },
    }
    .with_header("cache-control", cache)
    .with_header("x-content-type-options", "nosniff");
    match pushed_sha256 {
        Some(sha) => reply.with_header("etag", format!("\"{sha}\"")),
        None => reply,
    }
}
