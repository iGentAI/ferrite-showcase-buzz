//! The web tools: configuration, egress and the search and browser providers.

use ferrite_app::serde_json;
use ferrite_app::serde_json::Map;
use ferrite_app::serde_json::Value;
use ferrite_app::EgressOutcome;
use crate::agents::error::Result;
use args::checked_query;
use args::merge_options;
use args::string_arg;
use args::u64_arg;
use args::validate_http_url;
use args::web_target_body;
use web::binary_json;
use web::cloudflare_crawl_status_url;
use web::cloudflare_headers;
use web::cloudflare_url;
use web::trim_brave_news_result;
use web::trim_brave_web_result;
use web::trim_perplexity_result;
use web::unwrap_cloudflare_envelope;
use web::with_query;
use web::WebBinding;
use web::WebConfigDoc;
use web::DEFAULT_BRAVE_NEWS_URL;
use web::DEFAULT_BRAVE_WEB_URL;
use web::DEFAULT_CLOUDFLARE_API_BASE;
use web::DEFAULT_PERPLEXITY_CHAT_URL;
use web::DEFAULT_PERPLEXITY_SEARCH_URL;
use web::WEB_BRAVE_KEY;
use web::WEB_BRAVE_NEWS_URL;
use web::WEB_BRAVE_WEB_URL;
use web::WEB_CLOUDFLARE_ACCOUNT_ID;
use web::WEB_CLOUDFLARE_API_BASE;
use web::WEB_CLOUDFLARE_BROWSER_TOKEN;
use web::WEB_PERPLEXITY_CHAT_URL;
use web::WEB_PERPLEXITY_KEY;
use web::WEB_PERPLEXITY_SEARCH_URL;
use super::{CloudToolRegistry, args, excerpt_bytes, perplexity_headers, web};

impl<'a> CloudToolRegistry<'a> {
    pub(super) async fn web_config(&self) -> Result<Option<WebConfigDoc>> {
        let Some(web) = &self.web else {
            return Ok(None);
        };
        let mut store = self.ctx.store(&web.config_store_handle)?;
        let read = |raw: Option<Vec<u8>>| {
            raw.map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
                .filter(|s| !s.is_empty())
        };
        // A deployed app seals credentials with set_secret; the config store
        // is the fallback (tests and older deployments seed it directly).
        let secret = |name: &[u8]| {
            std::str::from_utf8(name)
                .ok()
                .and_then(|name| self.ctx.secret(name))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        };
        let brave_key = secret(WEB_BRAVE_KEY).or(read(store.get(WEB_BRAVE_KEY).await?));
        let brave_web_url = read(store.get(WEB_BRAVE_WEB_URL).await?)
            .unwrap_or_else(|| DEFAULT_BRAVE_WEB_URL.to_string());
        let brave_news_url = read(store.get(WEB_BRAVE_NEWS_URL).await?)
            .unwrap_or_else(|| DEFAULT_BRAVE_NEWS_URL.to_string());
        let perplexity_key =
            secret(WEB_PERPLEXITY_KEY).or(read(store.get(WEB_PERPLEXITY_KEY).await?));
        let perplexity_chat_url = read(store.get(WEB_PERPLEXITY_CHAT_URL).await?)
            .unwrap_or_else(|| DEFAULT_PERPLEXITY_CHAT_URL.to_string());
        let perplexity_search_url = read(store.get(WEB_PERPLEXITY_SEARCH_URL).await?)
            .unwrap_or_else(|| DEFAULT_PERPLEXITY_SEARCH_URL.to_string());
        let cloudflare_account_id = secret(WEB_CLOUDFLARE_ACCOUNT_ID)
            .or(read(store.get(WEB_CLOUDFLARE_ACCOUNT_ID).await?));
        let cloudflare_browser_token = secret(WEB_CLOUDFLARE_BROWSER_TOKEN)
            .or(read(store.get(WEB_CLOUDFLARE_BROWSER_TOKEN).await?));
        let cloudflare_api_base = read(store.get(WEB_CLOUDFLARE_API_BASE).await?)
            .unwrap_or_else(|| DEFAULT_CLOUDFLARE_API_BASE.to_string());
        Ok(Some(WebConfigDoc {
            brave_key,
            brave_web_url,
            brave_news_url,
            perplexity_key,
            perplexity_chat_url,
            perplexity_search_url,
            cloudflare_account_id,
            cloudflare_browser_token,
            cloudflare_api_base,
        }))
    }

    pub(super) async fn invoke_web_tool(&self, tool: &str, args: &[u8]) -> Result<Vec<u8>> {
        let Some(web) = &self.web else {
            return Self::err("web egress is not bound");
        };
        let Some(cfg) = self.web_config().await? else {
            return Self::err("web config store is not bound");
        };
        let args: Value = serde_json::from_slice(args).unwrap_or_else(|_| serde_json::json!({}));
        let value = match self.dispatch_web_tool(web, &cfg, tool, &args).await {
            Ok(value) => value,
            Err(err) => serde_json::json!({"error": err, "kind": "web"}),
        };
        Self::ok(value)
    }

    pub(super) async fn dispatch_web_tool(
        &self,
        web: &WebBinding,
        cfg: &WebConfigDoc,
        tool: &str,
        args: &Value,
    ) -> std::result::Result<Value, String> {
        match tool {
            "web_search" => self.brave_search(web, cfg, args, false).await,
            "news_search" => self.brave_search(web, cfg, args, true).await,
            "perplexity_ask" => self.perplexity_ask(web, cfg, args).await,
            "perplexity_search" => self.perplexity_search(web, cfg, args).await,
            "browser_content" => {
                self.browser_json_action(web, cfg, "content", args, &[], "html")
                    .await
            }
            "browser_markdown" => {
                self.browser_json_action(web, cfg, "markdown", args, &[], "markdown")
                    .await
            }
            "browser_links" => {
                self.browser_json_action(
                    web,
                    cfg,
                    "links",
                    args,
                    &["visibleLinksOnly", "excludeExternalLinks"],
                    "links",
                )
                .await
            }
            "browser_scrape" => {
                let body = web_target_body(args, &["elements"])?;
                if body
                    .get("elements")
                    .and_then(Value::as_array)
                    .is_none_or(|items| items.is_empty())
                {
                    return Err("browser_scrape requires a non-empty elements array".to_string());
                }
                self.cloudflare_json_action(web, cfg, "scrape", &body, "elements")
                    .await
            }
            "browser_json" => {
                let body = web_target_body(args, &["prompt", "response_format"])?;
                if body.get("prompt").is_none() && body.get("response_format").is_none() {
                    return Err("browser_json requires prompt or response_format".to_string());
                }
                self.cloudflare_json_action(web, cfg, "json", &body, "data")
                    .await
            }
            "browser_snapshot" => {
                let body = web_target_body(args, &["formats"])?;
                self.cloudflare_json_raw(web, cfg, "snapshot", &body).await
            }
            "browser_screenshot" => {
                let body = web_target_body(args, &["screenshotOptions", "selector"])?;
                let bytes = self.cloudflare_bytes(web, cfg, "screenshot", &body).await?;
                let content_type = body
                    .pointer("/screenshotOptions/type")
                    .and_then(Value::as_str)
                    .map(|kind| {
                        if kind.eq_ignore_ascii_case("jpeg") {
                            "image/jpeg"
                        } else {
                            "image/png"
                        }
                    })
                    .unwrap_or("image/png");
                Ok(binary_json(content_type, &bytes))
            }
            "browser_pdf" => {
                let body = web_target_body(args, &["pdfOptions"])?;
                let bytes = self.cloudflare_bytes(web, cfg, "pdf", &body).await?;
                Ok(binary_json("application/pdf", &bytes))
            }
            "browser_crawl" => {
                let url = string_arg(args, "url")
                    .ok_or_else(|| "browser_crawl requires url".to_string())?;
                validate_http_url(&url)?;
                let mut body = Map::new();
                body.insert("url".to_string(), serde_json::json!(url));
                if let Some(options) = args.get("options") {
                    merge_options(&mut body, options)?;
                }
                let job_id = self
                    .cloudflare_json_raw(web, cfg, "crawl", &Value::Object(body))
                    .await?;
                if let Some(id) = job_id.as_str() {
                    self.record_crawl_job(id).await?;
                }
                Ok(serde_json::json!({"job_id": job_id}))
            }
            "browser_crawl_status" => self.browser_crawl_status(web, cfg, args).await,
            "browser_automate" => self.browser_automate(web, cfg, args).await,
            other => Err(format!("unknown web tool: {other}")),
        }
    }

    pub(super) async fn egress_deliver(
        &self,
        method: ferrite_app::EgressHttpMethod,
        egress_handle: &str,
        destination: &str,
        body: &[u8],
        headers: &[(String, String)],
    ) -> std::result::Result<Vec<u8>, String> {
        let mut egress = self.ctx.egress(egress_handle).map_err(|e| e.to_string())?;
        match egress
            .deliver_http(
                method,
                destination,
                &crate::egress_headers(headers),
                body,
            )
            .await
            .map_err(|e| e.to_string())?
        {
            EgressOutcome::Delivered { status, body, .. } if (200..300).contains(&status) => {
                Ok(body)
            }
            EgressOutcome::Delivered { status, body, .. } => {
                Err(format!("provider error {status}: {}", excerpt_bytes(&body)))
            }
            EgressOutcome::Denied { reason } => Err(format!("egress denied: {reason}")),
        }
    }

    pub(super) async fn egress_post(
        &self,
        egress_handle: &str,
        destination: &str,
        body: &[u8],
        headers: &[(String, String)],
    ) -> std::result::Result<Vec<u8>, String> {
        self.egress_deliver(ferrite_app::EgressHttpMethod::Post, egress_handle, destination, body, headers)
            .await
    }

    pub(super) async fn egress_get(
        &self,
        egress_handle: &str,
        destination: &str,
        headers: &[(String, String)],
    ) -> std::result::Result<Vec<u8>, String> {
        self.egress_deliver(ferrite_app::EgressHttpMethod::Get, egress_handle, destination, &[], headers)
            .await
    }

    pub(super) async fn egress_json(
        &self,
        egress_handle: &str,
        destination: &str,
        body: &[u8],
        headers: &[(String, String)],
    ) -> std::result::Result<Value, String> {
        let bytes = self
            .egress_post(egress_handle, destination, body, headers)
            .await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| format!("decode json: {e}: {}", excerpt_bytes(&bytes)))
    }

    pub(super) async fn egress_get_json(
        &self,
        egress_handle: &str,
        destination: &str,
        headers: &[(String, String)],
    ) -> std::result::Result<Value, String> {
        let bytes = self.egress_get(egress_handle, destination, headers).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| format!("decode json: {e}: {}", excerpt_bytes(&bytes)))
    }

    pub(super) async fn brave_search(
        &self,
        web: &WebBinding,
        cfg: &WebConfigDoc,
        args: &Value,
        news: bool,
    ) -> std::result::Result<Value, String> {
        let key = cfg.brave_key.as_deref().ok_or_else(|| {
            format!(
                "missing credential: {}",
                String::from_utf8_lossy(WEB_BRAVE_KEY)
            )
        })?;
        let query = checked_query(args, "query", 400)?;
        let max = if news { 50 } else { 20 };
        let count = u64_arg(args, "count").unwrap_or(10).clamp(1, max);
        let mut params = vec![
            ("q".to_string(), query.clone()),
            ("count".to_string(), count.to_string()),
        ];
        for key in ["country", "freshness", "safesearch"] {
            if let Some(value) = string_arg(args, key) {
                params.push((key.to_string(), value));
            }
        }
        let base = if news {
            &cfg.brave_news_url
        } else {
            &cfg.brave_web_url
        };
        let destination = with_query(base, &params);
        let headers = vec![
            ("x-subscription-token".to_string(), key.to_string()),
            ("accept".to_string(), "application/json".to_string()),
        ];
        let raw = self
            .egress_get_json(&web.egress_handle, &destination, &headers)
            .await?;
        let results = if news {
            raw.get("results")
                .and_then(Value::as_array)
                .map(|items| items.iter().map(trim_brave_news_result).collect::<Vec<_>>())
                .unwrap_or_default()
        } else {
            raw.get("web")
                .and_then(|web| web.get("results"))
                .and_then(Value::as_array)
                .map(|items| items.iter().map(trim_brave_web_result).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        Ok(serde_json::json!({"query": query, "count": results.len(), "results": results}))
    }

    pub(super) async fn perplexity_ask(
        &self,
        web: &WebBinding,
        cfg: &WebConfigDoc,
        args: &Value,
    ) -> std::result::Result<Value, String> {
        let key = cfg.perplexity_key.as_deref().ok_or_else(|| {
            format!(
                "missing credential: {}",
                String::from_utf8_lossy(WEB_PERPLEXITY_KEY)
            )
        })?;
        let query = checked_query(args, "query", 8000)?;
        let model = string_arg(args, "model").unwrap_or_else(|| "sonar".to_string());
        if !matches!(
            model.as_str(),
            "sonar" | "sonar-pro" | "sonar-reasoning-pro" | "sonar-deep-research"
        ) {
            return Err(format!("unknown perplexity model '{model}'"));
        }
        let mut body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": query}],
        });
        let obj = body.as_object_mut().expect("object");
        if let Some(value) = string_arg(args, "recency") {
            obj.insert("search_recency_filter".to_string(), serde_json::json!(value));
        }
        if let Some(value) = args.get("domains").and_then(Value::as_array) {
            obj.insert(
                "search_domain_filter".to_string(),
                Value::Array(value.clone()),
            );
        }
        if let Some(value) = string_arg(args, "search_mode") {
            obj.insert("search_mode".to_string(), serde_json::json!(value));
        }
        if let Some(value) = u64_arg(args, "max_tokens") {
            obj.insert("max_tokens".to_string(), serde_json::json!(value));
        }
        let headers = perplexity_headers(key);
        let raw = self
            .egress_json(
                &web.egress_handle,
                &cfg.perplexity_chat_url,
                body.to_string().as_bytes(),
                &headers,
            )
            .await?;
        let answer = raw
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let citations = raw.get("citations").cloned().unwrap_or_else(|| serde_json::json!([]));
        let results = raw
            .get("search_results")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(trim_perplexity_result).collect::<Vec<_>>())
            .unwrap_or_default();
        Ok(serde_json::json!({"answer": answer, "model": model, "citations": citations, "results": results}))
    }

    pub(super) async fn perplexity_search(
        &self,
        web: &WebBinding,
        cfg: &WebConfigDoc,
        args: &Value,
    ) -> std::result::Result<Value, String> {
        let key = cfg.perplexity_key.as_deref().ok_or_else(|| {
            format!(
                "missing credential: {}",
                String::from_utf8_lossy(WEB_PERPLEXITY_KEY)
            )
        })?;
        let query = checked_query(args, "query", 8000)?;
        let body = serde_json::json!({
            "query": query,
            "max_results": u64_arg(args, "max_results").unwrap_or(10).clamp(1, 20),
        });
        let headers = perplexity_headers(key);
        let raw = self
            .egress_json(
                &web.egress_handle,
                &cfg.perplexity_search_url,
                body.to_string().as_bytes(),
                &headers,
            )
            .await?;
        let results = raw
            .get("results")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(trim_perplexity_result).collect::<Vec<_>>())
            .unwrap_or_default();
        Ok(serde_json::json!({"query": query, "count": results.len(), "results": results}))
    }

    pub(super) async fn browser_json_action(
        &self,
        web: &WebBinding,
        cfg: &WebConfigDoc,
        endpoint: &str,
        args: &Value,
        fold: &[&str],
        result_key: &str,
    ) -> std::result::Result<Value, String> {
        let body = web_target_body(args, fold)?;
        self.cloudflare_json_action(web, cfg, endpoint, &body, result_key)
            .await
    }

    pub(super) async fn cloudflare_json_action(
        &self,
        web: &WebBinding,
        cfg: &WebConfigDoc,
        endpoint: &str,
        body: &Value,
        result_key: &str,
    ) -> std::result::Result<Value, String> {
        let value = self.cloudflare_json_raw(web, cfg, endpoint, body).await?;
        Ok(serde_json::json!({ result_key: value }))
    }

    pub(super) async fn cloudflare_json_raw(
        &self,
        web: &WebBinding,
        cfg: &WebConfigDoc,
        endpoint: &str,
        body: &Value,
    ) -> std::result::Result<Value, String> {
        let raw = self
            .egress_json(
                &web.egress_handle,
                &cloudflare_url(cfg, endpoint)?,
                body.to_string().as_bytes(),
                &cloudflare_headers(cfg)?,
            )
            .await?;
        unwrap_cloudflare_envelope(raw)
    }

    pub(super) async fn cloudflare_bytes(
        &self,
        web: &WebBinding,
        cfg: &WebConfigDoc,
        endpoint: &str,
        body: &Value,
    ) -> std::result::Result<Vec<u8>, String> {
        self.egress_post(
            &web.egress_handle,
            &cloudflare_url(cfg, endpoint)?,
            body.to_string().as_bytes(),
            &cloudflare_headers(cfg)?,
        )
        .await
    }

    /// Record that this conversation started crawl job `job_id`.
    pub(super) async fn record_crawl_job(&self, job_id: &str) -> std::result::Result<(), String> {
        let key = super::crawl_job_key(&self.scope, job_id)
            .ok_or_else(|| "the crawl service returned an unusable job id".to_string())?;
        let mut store = self.ctx.store(&self.store_handle).map_err(|e| e.to_string())?;
        store.put(&key, b"1").await.map_err(|e| e.to_string())?;
        Ok(())
    }

    /// `Ok` when this conversation started crawl job `job_id`; otherwise the
    /// refusal the model sees. Another conversation's job id, learned from a
    /// private channel or guessed, is never polled with the shared token.
    pub(super) async fn crawl_job_owned(&self, job_id: &str) -> std::result::Result<(), String> {
        let unknown = || {
            format!("unknown job_id {job_id:?}: only crawls started by browser_crawl in this conversation can be polled")
        };
        let Some(key) = super::crawl_job_key(&self.scope, job_id) else {
            return Err(unknown());
        };
        let mut store = self.ctx.store(&self.store_handle).map_err(|e| e.to_string())?;
        match store.get(&key).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(unknown()),
            Err(e) => Err(format!("crawl job lookup failed: {e}")),
        }
    }

    pub(super) async fn browser_crawl_status(
        &self,
        web: &WebBinding,
        cfg: &WebConfigDoc,
        args: &Value,
    ) -> std::result::Result<Value, String> {
        let job_id = string_arg(args, "job_id")
            .ok_or_else(|| "browser_crawl_status requires job_id".to_string())?;
        self.crawl_job_owned(&job_id).await?;
        let mut destination = cloudflare_crawl_status_url(cfg, &job_id)?;
        let mut params = Vec::new();
        if let Some(cursor) = u64_arg(args, "cursor") {
            params.push(("cursor".to_string(), cursor.to_string()));
        }
        if let Some(limit) = u64_arg(args, "limit") {
            params.push(("limit".to_string(), limit.to_string()));
        }
        if !params.is_empty() {
            destination = with_query(&destination, &params);
        }
        let raw = self
            .egress_get_json(&web.egress_handle, &destination, &cloudflare_headers(cfg)?)
            .await?;
        unwrap_cloudflare_envelope(raw)
    }

    pub(super) async fn browser_automate(
        &self,
        _web: &WebBinding,
        _cfg: &WebConfigDoc,
        _args: &Value,
    ) -> std::result::Result<Value, String> {
        Err(
            "browser_automate is unavailable on clean upstream Ferrite: the permitted gateway patch adds HTTP GET and request headers, but not outbound WebSocket egress"
                .to_string(),
        )
    }
}
