//! The persona contract at ingest.
//!
//! kind 30175 is buzz's own product kind, and its rules are a PRODUCT contract
//! rather than a NIP: NIP-33 d-tags are arbitrary text by specification, so the
//! replaceable-event machinery would never refuse any of the shapes below. Before
//! this gate the relay accepted every one of them with `OK true` and then ignored
//! them, which is the worst way to answer a request — an author has no way to tell
//! that what they asked for is not what they got.
//!
//! Driven through the real ingest route, and every refusal is checked to have
//! stored NOTHING: a refusal that still wrote the event would be worse than the
//! silent acceptance it replaced.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::common;

use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::kind;
use common::{Actor, gate_env, error_text, signed, stored_event, submit, tag};

/// A persona event with whatever tags the case under test wants.
fn persona(actor: &Actor, tags: Vec<Vec<String>>) -> Event {
    signed(
        actor,
        kind::KIND_PERSONA,
        tags,
        "{\"instructions\":\"be helpful\"}",
    )
}

/// **A persona d-tag must be a slug, and anything else is refused rather than
/// stored.**
///
/// The d-tag is the persona's stable identity inside its author's namespace: a
/// client renders it, a URL carries it, an operator types it. Every shape below
/// used to be accepted.
#[test]
 fn severe_a_persona_d_tag_must_be_a_slug() {
    common::run_scenario(|| async move {
    let gate = gate_env().await;
    let author = Actor::new("persona-author");

    let cases: Vec<(&str, Vec<Vec<String>>)> = vec![
        ("no d tag at all", vec![]),
        ("an empty d tag", vec![tag("d", "")]),
        ("a leading underscore", vec![tag("d", "_reserved")]),
        ("uppercase", vec![tag("d", "Assistant")]),
        ("punctuation", vec![tag("d", "my.persona!")]),
        ("a space", vec![tag("d", "my persona")]),
        ("non-ascii", vec![tag("d", "pers\u{f6}na")]),
        ("65 characters", vec![tag("d", &"a".repeat(65))]),
    ];

    for (label, tags) in cases {
        let event = persona(&author, tags);
        let reply = submit(&gate, &author, &event).await;
        assert_eq!(
            reply.status,
            400,
            "{label} was accepted: {}",
            String::from_utf8_lossy(&reply.body)
        );
        assert!(
            error_text(&reply).contains("d tag"),
            "{label} must be refused for its d tag, got {:?}",
            error_text(&reply)
        );
        assert!(
            stored_event(&gate, &event.id).await.is_none(),
            "{label} was refused but stored anyway"
        );
    }

    // The boundary and the ordinary case both pass, so the rule is a rule and not
    // a blanket refusal.
    for good in ["a", "triage-bot", "agent_2", &"a".repeat(64)] {
        let event = persona(&author, vec![tag("d", good)]);
        let reply = submit(&gate, &author, &event).await;
        assert_eq!(
            reply.status,
            200,
            "{good:?} should be a valid slug: {}",
            String::from_utf8_lossy(&reply.body)
        );
        assert!(
            stored_event(&gate, &event.id).await.is_some(),
            "{good:?} was accepted but not stored"
        );
    }

    });
}

/// **A persona `shared` tag has exactly one meaningful spelling.**
///
/// The read path treats precisely `["shared","true"]` as "foreign readers may see
/// this" and everything else as unshared. Accepting another spelling accepts a tag
/// that reads as though it had been understood — so an author who writes
/// `["shared","false"]` believing it means something is told, rather than quietly
/// getting the same result they would have got by omitting it.
#[test]
 fn severe_a_persona_shared_tag_has_one_spelling() {
    common::run_scenario(|| async move {
    let gate = gate_env().await;
    let author = Actor::new("shared-author");

    let refused: Vec<(&str, Vec<String>)> = vec![
        ("false", tag("shared", "false")),
        ("an empty value", tag("shared", "")),
        ("True", tag("shared", "True")),
        (
            "a third element",
            vec!["shared".into(), "true".into(), "extra".into()],
        ),
        ("no value at all", vec!["shared".into()]),
    ];

    for (label, shared) in refused {
        let event = persona(&author, vec![tag("d", "sharing-bot"), shared]);
        let reply = submit(&gate, &author, &event).await;
        assert_eq!(
            reply.status,
            400,
            "a shared tag with {label} was accepted: {}",
            String::from_utf8_lossy(&reply.body)
        );
        assert!(
            error_text(&reply).contains("shared tag"),
            "{label} must be refused for its shared tag, got {:?}",
            error_text(&reply)
        );
        assert!(
            stored_event(&gate, &event.id).await.is_none(),
            "a shared tag with {label} was refused but stored anyway"
        );
    }

    // The one spelling that means something, and the absence of the tag, both pass.
    for tags in [
        vec![tag("d", "shared-yes"), tag("shared", "true")],
        vec![tag("d", "shared-absent")],
    ] {
        let event = persona(&author, tags);
        let reply = submit(&gate, &author, &event).await;
        assert_eq!(
            reply.status,
            200,
            "{} was refused: {}",
            to_hex(&event.id),
            String::from_utf8_lossy(&reply.body)
        );
        assert!(stored_event(&gate, &event.id).await.is_some());
    }

    });
}
