//! **M1.13 — discovery reads (partial): NIP-11 relay-info + NIP-05.**
//!
//! Both are public reads. relay-info advertises the workspace's limits; NIP-05
//! resolves an operator-registered name to a pubkey and returns an empty set for
//! anything unregistered (never an error, never a distinguishing signal).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use common::{Actor, body_json, gate_env};
use ferrite_app::serde_json::json;
use ferrite_app_test::ingress;
use common::SsoTestEnv as TestEnv;

async fn get(env: &TestEnv, path: &str) -> ferrite_app::IngressReply {
    env.handle(common::get(path)).await.expect("reply")
}

#[test]
 fn severe_relay_info_advertises_limits() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let reply = get(&env, "/buzz/api/relay-info").await;
    assert_eq!(reply.status, 200);
    let info = body_json(&reply);
    assert_eq!(info["software"], json!("buzz"));
    assert_eq!(info["limitation"]["auth_required"], json!(true));
    assert!(
        info["supported_nips"]
            .as_array()
            .unwrap()
            .contains(&json!(29)),
        "NIP-29 (groups) is advertised"
    );

    });
}

#[test]
 fn severe_nip05_resolves_a_registered_name_and_empties_an_unknown() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    // The operator registers alice under the name "alice".
    {
        let mut config = env.store("config");
        config
            .put(
                b"buzz-nip05-names",
                json!({ "alice": alice.pk_hex() }).to_string().as_bytes(),
            )
            .await
            .expect("seed nip05 names");
    }

    let hit = body_json(&get(&env, "/buzz/api/nip05?name=alice").await);
    assert_eq!(
        hit["names"]["alice"],
        json!(alice.pk_hex()),
        "registered name resolves"
    );

    let miss = body_json(&get(&env, "/buzz/api/nip05?name=nobody").await);
    assert!(
        miss["names"].as_object().unwrap().is_empty(),
        "an unknown name returns an empty set, not an error: {miss}"
    );

    });
}
