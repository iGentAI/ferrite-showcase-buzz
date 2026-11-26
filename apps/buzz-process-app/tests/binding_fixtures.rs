//! Contract A, against CHECKED-IN fixtures.
//!
//! The fixtures under `tests/fixtures/binding-v1/` are static files whose
//! digests were computed by an INDEPENDENT implementation of the canonical
//! rule (`scratch/gen-binding-fixtures.py`). That is the point: if this
//! crate's canonicalization ever drifts — a key order, a field omission, a
//! digest input — the fixtures stop parsing, and the failure names the file.
//! A test that generated its own inputs could not catch that.

use std::path::PathBuf;

use buzz_process_app::binding::{Binding, Lifecycle};

const ORIGIN: &str = "https://buzz.example";

fn owner() -> String {
    "aa".repeat(32)
}

fn fixture(name: &str) -> Vec<u8> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures/binding-v1");
    path.push(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn the_valid_rcs_fixture_parses_and_is_ours() {
    let binding = Binding::parse(&fixture("valid-rcs.json")).expect("parse");
    assert_eq!(binding.provider, "ferrite-rcs");
    assert_eq!(binding.git_ref, "refs/heads/feature/x");
    assert_eq!(binding.generation, "g1");
    assert_eq!(binding.lifecycle, Lifecycle::Active);
    // The digest in the file was computed OUTSIDE this crate; parse verifies it
    // against this crate's own derivation, so equality here means the two
    // implementations of the canonical rule agree.
    let identity = binding.identity();
    assert_eq!(binding.binding_id, identity.binding_id().expect("id"));
    assert_eq!(binding.channel_id, identity.room_id().expect("room"));
    binding.accept_for(ORIGIN, &owner()).expect("ours and live");
}

#[test]
fn the_github_fork_fixture_keeps_its_head_repository() {
    let binding = Binding::parse(&fixture("valid-github-fork.json")).expect("parse");
    assert_eq!(binding.provider, "github");
    assert_eq!(binding.repository.stable_id, "12345");
    assert_eq!(binding.repository.head_stable_id.as_deref(), Some("999"));
    assert_eq!(binding.generation, "g2");
    // A fork binding is a DIFFERENT binding from the same ref in the base repo.
    let mut base = binding.identity();
    base.head_stable_id = None;
    assert_ne!(
        base.binding_id().expect("base id"),
        binding.binding_id,
        "a fork must not collide with its base repository"
    );
    binding.accept_for(ORIGIN, &owner()).expect("ours and live");
}

#[test]
fn an_archived_fixture_parses_but_is_not_accepted() {
    // Parsing and ACCEPTING are different questions: the document is
    // well-formed, and it is still refused for use.
    let binding = Binding::parse(&fixture("archived.json")).expect("parse");
    assert_eq!(binding.lifecycle, Lifecycle::Archived);
    assert!(binding.accept_for(ORIGIN, &owner()).is_err());
}

#[test]
fn a_foreign_binding_is_refused() {
    let scope = Binding::parse(&fixture("foreign-scope.json")).expect("parse");
    assert!(
        scope.accept_for(ORIGIN, &owner()).is_err(),
        "another origin"
    );
    let owner_mismatch = Binding::parse(&fixture("foreign-owner.json")).expect("parse");
    assert!(
        owner_mismatch.accept_for(ORIGIN, &owner()).is_err(),
        "another community owner"
    );
}

#[test]
fn every_malformed_fixture_is_refused_at_parse() {
    for (name, why) in [
        (
            "invalid-digest.json",
            "a binding_id that is not the identity's digest",
        ),
        (
            "invalid-room.json",
            "a channel_id not derived from the identity",
        ),
        ("invalid-unknown-field.json", "an unknown top-level field"),
        (
            "invalid-bare-ref.json",
            "a bare branch name instead of a full ref",
        ),
        (
            "invalid-visibility.json",
            "a visibility outside the closed vocabulary",
        ),
        (
            "invalid-owner-pubkey.json",
            "a community owner that is not a pubkey",
        ),
        (
            "invalid-schema.json",
            "a schema this version does not speak",
        ),
    ] {
        assert!(
            Binding::parse(&fixture(name)).is_err(),
            "{name} must be refused: {why}"
        );
    }
}
