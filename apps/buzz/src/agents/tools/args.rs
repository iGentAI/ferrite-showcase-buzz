use ferrite_app::serde_json::{self, Map, Value};

pub(super) fn string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

pub(super) fn u64_arg(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

pub(super) fn checked_query(
    args: &Value,
    key: &str,
    max_chars: usize,
) -> std::result::Result<String, String> {
    let query = string_arg(args, key).ok_or_else(|| format!("{key} must not be empty"))?;
    if query.chars().count() > max_chars {
        return Err(format!("{key} is too long (max {max_chars} chars)"));
    }
    Ok(query)
}

/// Validate that `url` is a plain `http`/`https` absolute URL naming a public
/// host: the only targets the browser tools accept. The same rules as
/// `ferrite-web`'s `validate_http_url`, which parses with the WHATWG parser
/// the rendering browser uses; this crate has no URL parser, so the host is
/// read the way that parser reads it, and anything it could read differently
/// is refused rather than guessed. Refused: another scheme; userinfo
/// (`http://evil.com@127.0.0.1/` dials `127.0.0.1`); a tab, newline, space or
/// other control byte (the WHATWG parser deletes a tab or newline, which
/// splices a host back together); a percent-encoded or non-ASCII host; and a
/// host in loopback, private, link-local, shared (CGNAT) or unique-local
/// space, including IPv4-mapped IPv6 and every IPv4 spelling the browser
/// accepts (`2130706433`, `0x7f.1`, `127.1`), or named `localhost`,
/// `*.localhost`, `*.internal` or `*.local`. A public DNS name that resolves
/// to private space cannot be judged here; that verdict belongs to the
/// fetcher at connect time.
pub(super) fn validate_http_url(url: &str) -> std::result::Result<(), String> {
    let trimmed = url.trim();
    if trimmed.bytes().any(|b| b.is_ascii_control() || b == b' ') {
        return Err(format!("URL must not contain whitespace or control characters: {url:?}"));
    }
    let lower = trimmed.to_ascii_lowercase();
    let Some(rest) = lower
        .strip_prefix("http:")
        .or_else(|| lower.strip_prefix("https:"))
    else {
        return Err(format!("only http/https URLs are allowed, got: {url:?}"));
    };
    // For an http(s) URL the WHATWG parser skips every `/` and `\` before the
    // authority and ends the authority at the next `/`, `\`, `?` or `#`.
    let rest = rest.trim_start_matches(['/', '\\']);
    let authority = rest
        .split(['/', '\\', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') {
        return Err(format!("URL must not carry userinfo: {url:?}"));
    }
    let host = match authority.strip_prefix('[') {
        Some(bracketed) => {
            let Some((v6, port)) = bracketed.split_once(']') else {
                return Err(format!("URL has an unterminated IPv6 host: {url:?}"));
            };
            check_port(port.strip_prefix(':').unwrap_or(port), url)?;
            let ip = parse_ipv6(v6)
                .ok_or_else(|| format!("URL has an invalid IPv6 host: {url:?}"))?;
            if is_internal_v6(ip) {
                return Err(format!("URL names an internal or private host: {url:?}"));
            }
            return Ok(());
        }
        None => match authority.split_once(':') {
            Some((host, port)) => {
                check_port(port, url)?;
                host
            }
            None => authority,
        },
    };
    if host.is_empty() {
        return Err(format!("URL has no valid host: {url:?}"));
    }
    if !host
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
    {
        return Err(format!(
            "URL host must be plain ASCII (use the xn-- form of an international name): {url:?}"
        ));
    }
    let internal = match whatwg_ipv4(host) {
        Some(Ok(ip)) => is_internal_v4(ip),
        Some(Err(())) => return Err(format!("URL has an invalid IPv4 host: {url:?}")),
        None => is_internal_name(host),
    };
    if internal {
        return Err(format!("URL names an internal or private host: {url:?}"));
    }
    Ok(())
}

fn check_port(port: &str, url: &str) -> std::result::Result<(), String> {
    if port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("URL has an invalid port: {url:?}"));
    }
    Ok(())
}

/// The WHATWG host parser's IPv4 reading: `None` when the host does not end
/// in a number (a DNS name), otherwise the address, or `Err` when it ends in
/// a number that is not a valid address (the browser refuses the URL).
///
/// Addresses are plain octets and segments, never the standard library's
/// network address types: the platform's code gate refuses a constrained app
/// that names that module at all (`CODE_AMBIENT_AUTHORITY`), value types
/// included, so the two parsers here are this crate's own.
fn whatwg_ipv4(host: &str) -> Option<std::result::Result<[u8; 4], ()>> {
    let mut labels: Vec<&str> = host.split('.').collect();
    if labels.len() > 1 && labels.last() == Some(&"") {
        labels.pop();
    }
    let last = labels.last()?;
    let ends_in_number = (!last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()))
        || last
            .strip_prefix("0x")
            .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()));
    if !ends_in_number {
        return None;
    }
    let parse = |label: &str| -> std::result::Result<u64, ()> {
        let (digits, radix) = if let Some(hex) = label.strip_prefix("0x") {
            (hex, 16)
        } else if label.len() > 1 && label.starts_with('0') {
            (&label[1..], 8)
        } else {
            (label, 10)
        };
        if digits.is_empty() {
            return if label.is_empty() { Err(()) } else { Ok(0) };
        }
        if digits.len() > 32 {
            return Err(());
        }
        u64::from_str_radix(digits, radix).map_err(|_| ())
    };
    let parsed = labels
        .iter()
        .map(|label| parse(label))
        .collect::<std::result::Result<Vec<u64>, ()>>();
    Some((|| {
        let numbers = parsed?;
        if numbers.len() > 4 {
            return Err(());
        }
        let (last, leading) = numbers.split_last().ok_or(())?;
        if leading.iter().any(|n| *n > 255) || *last >= 256u64.pow(5 - numbers.len() as u32) {
            return Err(());
        }
        let mut address = *last;
        for (i, n) in leading.iter().enumerate() {
            address += n << (8 * (3 - i));
        }
        Ok(u32::try_from(address).map_err(|_| ())?.to_be_bytes())
    })())
}

/// A dotted-quad IPv4 address in the strict form an IPv6 literal may end in:
/// exactly four decimal fields, each 0 to 255, no leading zero.
fn strict_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut fields = text.split('.');
    for octet in &mut octets {
        let field = fields.next()?;
        if field.is_empty()
            || field.len() > 3
            || !field.bytes().all(|b| b.is_ascii_digit())
            || (field.len() > 1 && field.starts_with('0'))
        {
            return None;
        }
        *octet = u8::try_from(field.parse::<u16>().ok()?).ok()?;
    }
    if fields.next().is_some() {
        return None;
    }
    Some(octets)
}

/// The text between the brackets of an IPv6 URL host, as eight segments
/// (RFC 4291 section 2.2): up to eight groups of one to four hex digits, at
/// most one `::`, and optionally a dotted-quad IPv4 address as the last 32
/// bits. A zone id, an empty group or a ninth group is refused.
fn parse_ipv6(text: &str) -> Option<[u16; 8]> {
    fn groups(part: &str, allow_v4_tail: bool) -> Option<Vec<u16>> {
        let mut out = Vec::new();
        if part.is_empty() {
            return Some(out);
        }
        let fields: Vec<&str> = part.split(':').collect();
        for (i, field) in fields.iter().enumerate() {
            if allow_v4_tail && i + 1 == fields.len() && field.contains('.') {
                let [a, b, c, d] = strict_ipv4(field)?;
                out.push(u16::from_be_bytes([a, b]));
                out.push(u16::from_be_bytes([c, d]));
                continue;
            }
            if field.is_empty() || field.len() > 4 || !field.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            out.push(u16::from_str_radix(field, 16).ok()?);
        }
        Some(out)
    }
    let mut segments = [0u16; 8];
    match text.split_once("::") {
        Some((head, tail)) => {
            if tail.contains("::") {
                return None;
            }
            let head = groups(head, false)?;
            let tail = groups(tail, true)?;
            // `::` stands for at least one zero group.
            if head.len() + tail.len() > 7 {
                return None;
            }
            segments[..head.len()].copy_from_slice(&head);
            segments[8 - tail.len()..].copy_from_slice(&tail);
        }
        None => {
            let all = groups(text, true)?;
            if all.len() != 8 {
                return None;
            }
            segments.copy_from_slice(&all);
        }
    }
    Some(segments)
}

fn is_internal_name(name: &str) -> bool {
    let name = name.trim_end_matches('.');
    name.is_empty()
        || name == "localhost"
        || name.ends_with(".localhost")
        || name.ends_with(".internal")
        || name.ends_with(".local")
}

fn is_internal_v4(ip: [u8; 4]) -> bool {
    let [a, b, _, _] = ip;
    // "This network" 0.0.0.0/8 (it holds the unspecified address).
    a == 0
        // Loopback 127.0.0.0/8.
        || a == 127
        // Private space (RFC 1918).
        || a == 10
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 168)
        // Link-local 169.254.0.0/16, which holds the cloud metadata address.
        || (a == 169 && b == 254)
        // Limited broadcast.
        || ip == [255, 255, 255, 255]
        // Shared address space (RFC 6598), used for carrier and cloud NAT.
        || (a == 100 && (64..=127).contains(&b))
}

fn is_internal_v6(seg: [u16; 8]) -> bool {
    let v4_tail = || {
        let [hi_a, hi_b] = seg[6].to_be_bytes();
        let [lo_a, lo_b] = seg[7].to_be_bytes();
        [hi_a, hi_b, lo_a, lo_b]
    };
    // Loopback `::1` and unspecified `::`.
    if seg == [0, 0, 0, 0, 0, 0, 0, 1] || seg == [0; 8] {
        return true;
    }
    // Unique-local fc00::/7, link-local fe80::/10, and deprecated site-local
    // fec0::/10 (still routed internally on some networks).
    if (seg[0] & 0xfe00) == 0xfc00 || (seg[0] & 0xffc0) == 0xfe80 || (seg[0] & 0xffc0) == 0xfec0 {
        return true;
    }
    // IPv4-mapped (::ffff:a.b.c.d), IPv4-compatible (::a.b.c.d) and the NAT64
    // well-known prefix (64:ff9b::/96) reach the embedded IPv4 address.
    if seg[..5] == [0, 0, 0, 0, 0] && (seg[5] == 0 || seg[5] == 0xffff) {
        return is_internal_v4(v4_tail());
    }
    if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return is_internal_v4(v4_tail());
    }
    false
}

/// Body keys that name the page and are set only from the validated target.
const RESERVED_OPTION_KEYS: [&str; 2] = ["url", "html"];

/// Merge passthrough `options` into a request body that already holds the
/// validated target. A `url` or `html` key in the options is refused: merged
/// last, it would replace the checked target with an unchecked one and defeat
/// both the URL validation and the exactly-one-of rule.
pub(super) fn merge_options(
    target: &mut Map<String, Value>,
    options: &Value,
) -> std::result::Result<(), String> {
    if options.is_null() {
        return Ok(());
    }
    let Some(options) = options.as_object() else {
        return Err("options must be a JSON object".to_string());
    };
    if let Some(key) = RESERVED_OPTION_KEYS
        .iter()
        .find(|key| options.contains_key(**key))
    {
        return Err(format!(
            "options must not set `{key}`; pass the page as the top-level url or html"
        ));
    }
    for (key, value) in options {
        target.insert(key.clone(), value.clone());
    }
    Ok(())
}

pub(super) fn web_target_body(args: &Value, fold: &[&str]) -> std::result::Result<Value, String> {
    let url = string_arg(args, "url");
    let html = string_arg(args, "html");
    match (&url, &html) {
        (Some(_), Some(_)) => {
            return Err("provide exactly one of url or html, not both".to_string())
        }
        (None, None) => return Err("provide one of url or html".to_string()),
        (Some(url), None) => validate_http_url(url)?,
        (None, Some(_)) => {}
    }
    let mut body = Map::new();
    if let Some(url) = url {
        body.insert("url".to_string(), serde_json::json!(url));
    }
    if let Some(html) = html {
        body.insert("html".to_string(), serde_json::json!(html));
    }
    if let Some(options) = args.get("options") {
        merge_options(&mut body, options)?;
    }
    for key in fold {
        if let Some(value) = args.get(*key) {
            if !value.is_null() {
                body.insert((*key).to_string(), value.clone());
            }
        }
    }
    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_validation_refuses_userinfo_and_internal_hosts() {
        for public in [
            "https://example.com",
            "https://example.com:8443/a?b=c@d",
            "HTTPS://Example.COM/path",
            "http://93.184.215.14/",
            "http://[2606:2800:21f:cb07:6820:80da:af6b:8b2c]/",
            "https://example.com./",
            "https://1.example.com/",
        ] {
            assert!(validate_http_url(public).is_ok(), "{public:?} must be accepted");
        }
        for hostile in [
            "http://evil.com@127.0.0.1/",
            "http://user:pass@example.com/",
            "http://169.254.169.254/latest/meta-data/",
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://localhost:9000/",
            "http://LOCALHOST./",
            "http://app.localhost/",
            "http://printer.local/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[64:ff9b::a9fe:a9fe]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
            "http://10.0.0.5/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://100.100.100.200/",
            "http://0.0.0.0/",
            "http://2130706433/",
            "http://0x7f.1/",
            "http://0177.0.0.1/",
            "http://127.1/",
            "http://127.0.0.1./",
            "http:\\\\127.0.0.1\\",
            "http:127.0.0.1/",
            "http://127.0.\n0.1/",
            "http://%31%32%37.0.0.1/",
            "http://1.2.3.256/",
            "data:text/html,<h1>x</h1>",
            "https://",
            "not a url",
        ] {
            assert!(validate_http_url(hostile).is_err(), "{hostile:?} must be refused");
        }
    }

    #[test]
    fn ipv6_literals_parse_to_the_segments_they_name() {
        for (text, segments) in [
            ("::", [0u16; 8]),
            ("::1", [0, 0, 0, 0, 0, 0, 0, 1]),
            ("1::", [1, 0, 0, 0, 0, 0, 0, 0]),
            ("2606:2800:21f:cb07:6820:80da:af6b:8b2c", [0x2606, 0x2800, 0x21f, 0xcb07, 0x6820, 0x80da, 0xaf6b, 0x8b2c]),
            ("FE80::A", [0xfe80, 0, 0, 0, 0, 0, 0, 0xa]),
            ("::ffff:127.0.0.1", [0, 0, 0, 0, 0, 0xffff, 0x7f00, 1]),
            ("::10.0.0.5", [0, 0, 0, 0, 0, 0, 0x0a00, 5]),
            ("64:ff9b::169.254.169.254", [0x64, 0xff9b, 0, 0, 0, 0, 0xa9fe, 0xa9fe]),
            ("1:2:3:4:5:6:7.8.9.10", [1, 2, 3, 4, 5, 6, 0x0708, 0x090a]),
            ("1:2:3:4:5:6:7::", [1, 2, 3, 4, 5, 6, 7, 0]),
        ] {
            assert_eq!(parse_ipv6(text), Some(segments), "{text:?}");
        }
        for invalid in [
            "", ":", ":::", "1::2::3", "1:2:3:4:5:6:7", "1:2:3:4:5:6:7:8:9", "1:2:3:4:5:6:7:8::",
            "::1:2:3:4:5:6:7:8", "12345::", "g::1", "fe80::1%eth0", ":1", "1:", "1:::2",
            "::127.0.0.1:1", "::1.2.3", "::1.2.3.4.5", "::256.0.0.1", "::01.2.3.4", "1:2:3:4:5:6:7:1.2.3.4",
            "::ffff:0x7f.0.0.1", " ::1",
        ] {
            assert_eq!(parse_ipv6(invalid), None, "{invalid:?} must not parse");
        }
    }

    #[test]
    fn internal_address_space_is_refused_in_every_spelling() {
        for internal in [
            "http://[::]/",
            "http://[0:0:0:0:0:0:0:1]/",
            "http://[::127.0.0.1]/",
            "http://[::ffff:10.1.2.3]/",
            "http://[::ffff:a9fe:a9fe]/",
            "http://[0:0:0:0:0:ffff:c0a8:101]/",
            "http://[64:ff9b::10.0.0.1]/",
            "http://[FC00::1]/",
            "http://[fdff:ffff::1]/",
            "http://[febf::1]/",
            "http://[::1]:8080/",
            "http://127.255.255.254/",
            "http://172.31.255.255/",
            "http://100.64.0.1/",
            "http://100.127.255.255/",
            "http://255.255.255.255/",
            "http://0.1.2.3/",
            "http://169.254.0.1/",
            "http://[fec0::1]/",
        ] {
            assert!(validate_http_url(internal).is_err(), "{internal:?} must be refused");
        }
        for public in [
            "http://[2001:4860:4860::8888]/",
            "http://[::ffff:8.8.8.8]/",
            "http://[64:ff9b::8.8.8.8]/",
            "http://[2001:db8::1]:8443/x",
            "http://172.15.255.255/",
            "http://172.32.0.1/",
            "http://100.63.255.255/",
            "http://100.128.0.1/",
            "http://169.253.1.1/",
            "http://192.167.1.1/",
            "http://126.255.255.255/",
            "http://128.0.0.1/",
        ] {
            assert!(validate_http_url(public).is_ok(), "{public:?} must be accepted");
        }
        for invalid in ["http://[::1/", "http://[fe80::1%25eth0]/", "http://[1:2:3]/", "http://[]/"] {
            assert!(validate_http_url(invalid).is_err(), "{invalid:?} must be refused");
        }
    }

    #[test]
    fn options_cannot_replace_the_validated_target() {
        for args in [
            serde_json::json!({ "url": "https://example.com", "options": { "url": "file:///etc/passwd" } }),
            serde_json::json!({ "html": "<p>x</p>", "options": { "url": "http://169.254.169.254/latest/meta-data/" } }),
            serde_json::json!({ "url": "https://example.com", "options": { "html": "<p>x</p>" } }),
        ] {
            let err = web_target_body(&args, &[]).unwrap_err();
            assert!(err.contains("options must not set"), "{args}: {err}");
        }
        let body = web_target_body(
            &serde_json::json!({ "url": "https://example.com", "options": { "gotoOptions": { "waitUntil": "load" } } }),
            &[],
        )
        .unwrap();
        assert_eq!(body["url"], "https://example.com");
        assert_eq!(body["gotoOptions"]["waitUntil"], "load");
        let mut crawl = Map::new();
        crawl.insert("url".into(), serde_json::json!("https://example.com"));
        assert!(merge_options(&mut crawl, &serde_json::json!({ "url": "http://10.0.0.1/", "limit": 5 })).is_err());
        assert_eq!(crawl["url"], "https://example.com");
    }
}
