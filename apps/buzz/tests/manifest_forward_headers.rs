//! The gateway forwards only the headers a route lists. Buzz authenticates its
//! API, MCP and ACP routes with NIP-98 or a bearer token in `Authorization`,
//! and only a retired edge used to copy that into `X-Buzz-Authorization`. A
//! route that lists only the app-specific header therefore refuses every
//! standard client that reaches Buzz directly (finding_c82633f9996c5c7a).

use ferrite_app::serde_json::{self, Value};

const AUTHENTICATED_PREFIXES: &[&str] = &["/buzz/api/", "/events", "/query", "/count", "/mcp", "/acp"];

#[test]
fn authenticated_routes_forward_the_standard_authorization_header() {
    let manifest: Value =
        serde_json::from_str(include_str!("../ferrite.json")).expect("ferrite.json parses");
    let mut routes = Vec::new();
    collect_routes(&manifest, &mut routes);
    for prefix in AUTHENTICATED_PREFIXES {
        let route = routes
            .iter()
            .find(|route| route.get("prefix").and_then(Value::as_str) == Some(prefix))
            .unwrap_or_else(|| panic!("ferrite.json declares no route for {prefix}"));
        let headers: Vec<&str> = route
            .get("forward_headers")
            .and_then(Value::as_array)
            .map(|list| list.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        for required in ["authorization", "x-buzz-authorization"] {
            assert!(
                headers.contains(&required),
                "route {prefix} must forward {required}, forwards {headers:?}"
            );
        }
    }
}

fn collect_routes<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            if map.contains_key("prefix") && map.contains_key("forward_headers") {
                out.push(value);
            }
            for child in map.values() {
                collect_routes(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect_routes(child, out);
            }
        }
        _ => {}
    }
}
