//! Contract A — `buzz.process.binding.v1`: the branch ⇔ room ⇔ repository
//! binding.
//!
//! One signed document ties three identities together, and every field of it
//! answers a question that would otherwise be guessed:
//!
//! * **Which repository?** A provider instance plus the provider's own STABLE
//!   id — never a display name. A GitHub repo can be renamed and transferred;
//!   its numeric id cannot. A NIP-34 address and a clone URL ride along as
//!   projections for humans and tools, and are never the identity.
//! * **Which line of work?** A FULL ref (`refs/heads/x`), never a bare branch
//!   name, plus a `generation` that changes when a deleted branch name is
//!   recreated — so an old room can never silently attach to a new branch
//!   lifetime.
//! * **Which room?** A channel id DERIVED from the identity, so the same
//!   branch incarnation always names the same room and two cells racing to
//!   materialize it converge instead of forking.
//!
//! `binding_id` is a SHA-256 over the canonical JSON of the identity object
//! alone — not the whole document — because display names, URLs, timestamps
//! and lifecycle all change while the identity does not. Canonical means
//! sorted keys, no whitespace, strings only (RFC 8785's rules reduce to
//! exactly that for this shape, which is why the shape is all strings).

use std::collections::BTreeMap;

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json::{self, Value};

use crate::crypto::sha256::sha256;
use crate::crypto::{from_hex32, to_hex};
use crate::error::{ProcessError, Result};

/// The schema every binding document declares.
pub const SCHEMA: &str = "buzz.process.binding.v1";
/// The tag naming a binding event's id.
pub const TAG_BINDING: &str = "process-binding";
/// The tag naming a binding event's schema.
pub const TAG_SCHEMA: &str = "process-binding-schema";
/// `provider` for a hosted Ferrite RCS repository.
pub const PROVIDER_RCS: &str = "ferrite-rcs";
/// `provider` for a GitHub repository.
pub const PROVIDER_GITHUB: &str = "github";

/// Longest accepted value for any identity component. Generous for refs,
/// bounded because these strings are hashed, stored, and rendered.
const MAX_COMPONENT: usize = 256;
/// Longest accepted URL-shaped projection value.
const MAX_URL: usize = 1024;
/// Largest accepted binding document. A binding is a handful of short strings;
/// anything approaching this is not one.
const MAX_DOCUMENT: usize = 8 * 1024;

/// The frozen identity object — the ONLY input to `binding_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Identity {
    pub provider: String,
    pub provider_instance: String,
    pub stable_id: String,
    /// GitHub fork bindings carry the stable id of the HEAD repository too;
    /// omitted entirely (not empty-stringed) when absent, so its presence
    /// changes the digest exactly when it changes the meaning.
    pub head_stable_id: Option<String>,
    pub git_ref: String,
    pub generation: String,
}

impl Identity {
    /// A Ferrite RCS identity: the account is the provider instance, the
    /// canonical app id is the repository, and there is no head repository.
    #[must_use]
    pub fn rcs(account: &str, app: &str, git_ref: &str, generation: &str) -> Self {
        Self {
            provider: PROVIDER_RCS.to_string(),
            provider_instance: account.to_string(),
            stable_id: app.to_string(),
            head_stable_id: None,
            git_ref: git_ref.to_string(),
            generation: generation.to_string(),
        }
    }

    /// A GitHub identity, by numeric repository id.
    #[must_use]
    pub fn github(
        instance: &str,
        repo_id: &str,
        head_repo_id: Option<&str>,
        git_ref: &str,
        generation: &str,
    ) -> Self {
        Self {
            provider: PROVIDER_GITHUB.to_string(),
            provider_instance: instance.to_string(),
            stable_id: repo_id.to_string(),
            head_stable_id: head_repo_id.map(str::to_string),
            git_ref: git_ref.to_string(),
            generation: generation.to_string(),
        }
    }

    /// Refuse an identity that cannot be hashed into a stable name.
    pub fn validate(&self) -> Result<()> {
        let refuse = |why: &str| Err(ProcessError::Invalid(format!("binding identity: {why}")));
        for (name, value) in [
            ("provider", &self.provider),
            ("provider_instance", &self.provider_instance),
            ("stable_id", &self.stable_id),
            ("ref", &self.git_ref),
            ("generation", &self.generation),
        ] {
            if value.is_empty() {
                return refuse(&format!("{name} is empty"));
            }
            if value.len() > MAX_COMPONENT {
                return refuse(&format!("{name} is over {MAX_COMPONENT} bytes"));
            }
            if value.bytes().any(|b| b < 0x20 || b == 0x7f) {
                return refuse(&format!("{name} carries control bytes"));
            }
        }
        if let Some(head) = &self.head_stable_id {
            if head.is_empty() || head.len() > MAX_COMPONENT {
                return refuse("head_stable_id is empty or too long");
            }
            if head.bytes().any(|b| b < 0x20 || b == 0x7f) {
                return refuse("head_stable_id carries control bytes");
            }
        }
        // A FULL ref, never a bare branch name: `refs/heads/main` and `main`
        // are different strings and only one of them is unambiguous.
        if !self.git_ref.starts_with("refs/") {
            return refuse("ref must be a full ref (refs/heads/…)");
        }
        if !matches!(self.provider.as_str(), PROVIDER_RCS | PROVIDER_GITHUB) {
            return refuse("provider must be ferrite-rcs or github");
        }
        Ok(())
    }

    /// The canonical JSON the digest is taken over: sorted keys, no
    /// whitespace, every value a string. `BTreeMap` IS the sort.
    pub fn canonical_json(&self) -> Result<String> {
        let mut map: BTreeMap<&str, &str> = BTreeMap::new();
        map.insert("schema", SCHEMA);
        map.insert("provider", &self.provider);
        map.insert("provider_instance", &self.provider_instance);
        map.insert("stable_id", &self.stable_id);
        map.insert("ref", &self.git_ref);
        map.insert("generation", &self.generation);
        if let Some(head) = &self.head_stable_id {
            map.insert("head_stable_id", head);
        }
        serde_json::to_string(&map)
            .map_err(|e| ProcessError::Internal(format!("canonical identity: {e}")))
    }

    /// `sha256:<64-hex>` over [`Self::canonical_json`].
    pub fn binding_id(&self) -> Result<String> {
        self.validate()?;
        let digest = sha256(self.canonical_json()?.as_bytes());
        Ok(format!("sha256:{}", to_hex(&digest)))
    }

    /// The room this branch incarnation always names: `process-<64-hex>`,
    /// derived from the same canonical identity, so materialization is
    /// convergent rather than racy.
    pub fn room_id(&self) -> Result<String> {
        self.validate()?;
        let digest = sha256(self.canonical_json()?.as_bytes());
        Ok(format!("process-{}", to_hex(&digest)))
    }
}

/// Whether a binding is live or retired. Lifecycle changes are the cell's
/// effects; a READER refuses anything but `active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", rename_all = "lowercase")]
pub enum Lifecycle {
    Active,
    Archived,
}

/// Where the binding lives, and who owns it. Self-asserted — which is why a
/// reader compares every field against its own configuration and the
/// authoritative room readback rather than trusting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
pub struct Scope {
    pub buzz_origin: String,
    pub community_owner_pubkey: String,
    pub visibility: String,
}

/// The repository projection: identity plus the strings humans and tools use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
pub struct Repository {
    pub stable_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_stable_id: Option<String>,
    pub display: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nip34_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
pub struct Binding {
    pub schema: String,
    pub binding_id: String,
    pub channel_id: String,
    pub provider: String,
    pub provider_instance: String,
    pub repository: Repository,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub generation: String,
    pub lifecycle: Lifecycle,
    pub scope: Scope,
}

impl Binding {
    /// Build a binding from an identity, deriving `binding_id` and
    /// `channel_id` rather than accepting them — the two fields a caller
    /// must never be able to disagree with.
    pub fn new(
        identity: &Identity,
        display: &str,
        nip34_address: Option<&str>,
        clone_url: Option<&str>,
        web_url: Option<&str>,
        scope: Scope,
    ) -> Result<Self> {
        // The display name is not covered by the identity digest, so a long
        // repository or ref name is shortened (on a character boundary)
        // rather than producing a binding `parse` would later refuse.
        let mut cut = display.len().min(MAX_COMPONENT);
        while !display.is_char_boundary(cut) {
            cut -= 1;
        }
        let display = &display[..cut];
        let binding = Self {
            schema: SCHEMA.to_string(),
            binding_id: identity.binding_id()?,
            channel_id: identity.room_id()?,
            provider: identity.provider.clone(),
            provider_instance: identity.provider_instance.clone(),
            repository: Repository {
                stable_id: identity.stable_id.clone(),
                head_stable_id: identity.head_stable_id.clone(),
                display: display.to_string(),
                nip34_address: nip34_address.map(str::to_string),
                clone_url: clone_url.map(str::to_string),
                web_url: web_url.map(str::to_string),
            },
            git_ref: identity.git_ref.clone(),
            generation: identity.generation.clone(),
            lifecycle: Lifecycle::Active,
            scope,
        };
        // The same invariants `parse` enforces: a binding that could be
        // reserved but never read back would wedge its room.
        binding.check_projection()?;
        if from_hex32(&binding.scope.community_owner_pubkey).is_none() {
            return Err(ProcessError::Invalid(
                "scope.community_owner_pubkey is not a 32-byte hex pubkey".into(),
            ));
        }
        Ok(binding)
    }

    #[must_use]
    pub fn identity(&self) -> Identity {
        Identity {
            provider: self.provider.clone(),
            provider_instance: self.provider_instance.clone(),
            stable_id: self.repository.stable_id.clone(),
            head_stable_id: self.repository.head_stable_id.clone(),
            git_ref: self.git_ref.clone(),
            generation: self.generation.clone(),
        }
    }

    /// The bounded canonical document a binding event carries as content.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| ProcessError::Internal(format!("binding encode: {e}")))
    }

    /// The tags a binding event carries, so one deterministic query finds it.
    #[must_use]
    pub fn tags(&self) -> Vec<Vec<String>> {
        vec![
            vec![TAG_BINDING.to_string(), self.binding_id.clone()],
            vec![TAG_SCHEMA.to_string(), SCHEMA.to_string()],
            vec!["h".to_string(), self.channel_id.clone()],
        ]
    }

    /// Parse STRICTLY and verify self-consistency: the declared schema, the
    /// derived `binding_id`, and the derived `channel_id` must all agree with
    /// the identity the document states. A document that disagrees with its
    /// own digest is refused rather than repaired — there is no way to tell
    /// which half was meant. Unknown fields, oversized documents, and
    /// unbounded projection strings are refused too: this document is read
    /// from a room by anything that can post there, so "bounded" has to mean
    /// bounded in every direction.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_DOCUMENT {
            return Err(ProcessError::Invalid(format!(
                "binding document is over {MAX_DOCUMENT} bytes"
            )));
        }
        let binding: Self = serde_json::from_slice(bytes)
            .map_err(|e| ProcessError::Invalid(format!("binding document: {e}")))?;
        if binding.schema != SCHEMA {
            return Err(ProcessError::Invalid(format!(
                "binding schema is {:?}, expected {SCHEMA}",
                binding.schema
            )));
        }
        let identity = binding.identity();
        identity.validate()?;
        binding.check_projection()?;
        if binding.binding_id != identity.binding_id()? {
            return Err(ProcessError::Invalid(
                "binding_id is not the digest of this document's identity".into(),
            ));
        }
        if binding.channel_id != identity.room_id()? {
            return Err(ProcessError::Invalid(
                "channel_id is not derived from this document's identity".into(),
            ));
        }
        if from_hex32(&binding.scope.community_owner_pubkey).is_none() {
            return Err(ProcessError::Invalid(
                "scope.community_owner_pubkey is not a 32-byte hex pubkey".into(),
            ));
        }
        Ok(binding)
    }

    /// Bound every string the identity digest does NOT cover — display names,
    /// URLs, and scope — because those are exactly the fields a hostile
    /// document can inflate without changing its own `binding_id`.
    fn check_projection(&self) -> Result<()> {
        let refuse = |why: &str| Err(ProcessError::Invalid(format!("binding document: {why}")));
        let bounded = |name: &str, value: &str, max: usize| {
            if value.len() > max {
                Err(ProcessError::Invalid(format!(
                    "binding document: {name} is over {max} bytes"
                )))
            } else if value.bytes().any(|b| b < 0x20 || b == 0x7f) {
                Err(ProcessError::Invalid(format!(
                    "binding document: {name} carries control bytes"
                )))
            } else {
                Ok(())
            }
        };
        if self.repository.display.is_empty() {
            return refuse("repository.display is empty");
        }
        bounded(
            "repository.display",
            &self.repository.display,
            MAX_COMPONENT,
        )?;
        for (name, value) in [
            ("repository.nip34_address", &self.repository.nip34_address),
            ("repository.clone_url", &self.repository.clone_url),
            ("repository.web_url", &self.repository.web_url),
        ] {
            if let Some(value) = value {
                bounded(name, value, MAX_URL)?;
            }
        }
        bounded("scope.buzz_origin", &self.scope.buzz_origin, MAX_URL)?;
        bounded(
            "scope.community_owner_pubkey",
            &self.scope.community_owner_pubkey,
            64,
        )?;
        // The visibility vocabulary is closed: an unrecognized value would be
        // read as "not private" by anything comparing against "private".
        if !matches!(self.scope.visibility.as_str(), "private" | "public") {
            return refuse("scope.visibility must be private or public");
        }
        Ok(())
    }

    /// Accept this binding only if it is OURS and LIVE.
    ///
    /// Self-asserted scope never widens access: the origin and community owner
    /// must equal the configured ones, and an archived binding is refused so a
    /// retired room cannot be driven. A foreign binding is refused WITHOUT
    /// confirming whether the room it names exists.
    pub fn accept_for(&self, origin: &str, community_owner_pubkey: &str) -> Result<()> {
        if self.lifecycle != Lifecycle::Active {
            return Err(ProcessError::Refused("this binding is archived".into()));
        }
        if self.scope.buzz_origin != origin
            || !self
                .scope
                .community_owner_pubkey
                .eq_ignore_ascii_case(community_owner_pubkey)
        {
            return Err(ProcessError::Refused(
                "this binding belongs to another workspace".into(),
            ));
        }
        Ok(())
    }
}

/// The query that asks the relay whether it holds one binding event the cell
/// published: that event's `id`, kind 9 by `author` in room `channel_id`.
///
/// The relay serves an `ids` filter as a point read of the event's body
/// (`ws::plan_read` in `apps/buzz`), before any index walk, and re-checks the
/// other fields against what it finds. A tag query (`#h` with the binding's
/// tags) would not do: the relay answers it from the room's timeline index and
/// examines at most eight rounds of twice the limit, so about 130 ordinary
/// messages push the binding out of its answer. No volume of chat moves a
/// point read.
#[must_use]
pub fn binding_event_query(event_id: &str, channel_id: &str, author: &str) -> Value {
    serde_json::json!({
        "ids": [event_id],
        "kinds": [9],
        "authors": [author],
        "#h": [channel_id],
        "limit": 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            buzz_origin: "https://buzz.example".into(),
            community_owner_pubkey: "aa".repeat(32),
            visibility: "private".into(),
        }
    }

    fn rcs_identity() -> Identity {
        Identity::rcs("acct-1", "my-app", "refs/heads/feature/x", "gen-1")
    }

    #[test]
    fn the_identity_digest_is_stable_and_sorted() {
        let id = rcs_identity();
        let canonical = id.canonical_json().expect("canonical");
        // Sorted keys, no whitespace, strings only.
        assert_eq!(
            canonical,
            "{\"generation\":\"gen-1\",\"provider\":\"ferrite-rcs\",\
             \"provider_instance\":\"acct-1\",\"ref\":\"refs/heads/feature/x\",\
             \"schema\":\"buzz.process.binding.v1\",\"stable_id\":\"my-app\"}"
        );
        let binding_id = id.binding_id().expect("id");
        assert!(binding_id.starts_with("sha256:"));
        assert_eq!(binding_id.len(), "sha256:".len() + 64);
        // Deterministic: the same identity always names the same binding.
        assert_eq!(binding_id, rcs_identity().binding_id().expect("id"));
    }

    #[test]
    fn head_stable_id_changes_the_digest_only_when_present() {
        let without = Identity::github("github.com", "123", None, "refs/heads/x", "g");
        let with = Identity::github("github.com", "123", Some("456"), "refs/heads/x", "g");
        assert_ne!(
            without.binding_id().expect("id"),
            with.binding_id().expect("id"),
            "a fork binding is a different binding"
        );
        assert!(!without
            .canonical_json()
            .expect("c")
            .contains("head_stable_id"));
    }

    #[test]
    fn every_identity_component_changes_the_binding() {
        let base = rcs_identity().binding_id().expect("id");
        let variants = [
            Identity::rcs("acct-2", "my-app", "refs/heads/feature/x", "gen-1"),
            Identity::rcs("acct-1", "other-app", "refs/heads/feature/x", "gen-1"),
            Identity::rcs("acct-1", "my-app", "refs/heads/feature/y", "gen-1"),
            // The generation is the whole point: a recreated branch name is a
            // DIFFERENT incarnation and must not reuse the old room.
            Identity::rcs("acct-1", "my-app", "refs/heads/feature/x", "gen-2"),
        ];
        for v in variants {
            assert_ne!(base, v.binding_id().expect("id"), "{v:?}");
        }
    }

    #[test]
    fn the_room_id_is_derived_and_well_formed() {
        let room = rcs_identity().room_id().expect("room");
        assert!(room.starts_with("process-"));
        let hex = room.strip_prefix("process-").expect("prefix");
        assert_eq!(hex.len(), 64);
        assert!(hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

    #[test]
    fn a_bare_branch_name_is_refused() {
        let bad = Identity::rcs("a", "b", "main", "g");
        assert!(bad.validate().is_err(), "a bare branch name is ambiguous");
        assert!(Identity::rcs("a", "b", "refs/heads/main", "g")
            .validate()
            .is_ok());
    }

    #[test]
    fn identity_validation_refuses_junk() {
        for bad in [
            Identity::rcs("", "b", "refs/heads/m", "g"),
            Identity::rcs("a", "", "refs/heads/m", "g"),
            Identity::rcs("a", "b", "refs/heads/m", ""),
            Identity::rcs("a", "b", &format!("refs/heads/{}", "x".repeat(300)), "g"),
            Identity::rcs("a", "b", "refs/heads/\u{1}", "g"),
            Identity {
                provider: "gitlab".into(),
                provider_instance: "a".into(),
                stable_id: "b".into(),
                head_stable_id: None,
                git_ref: "refs/heads/m".into(),
                generation: "g".into(),
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_binding_round_trips_and_verifies_itself() {
        let id = rcs_identity();
        let binding = Binding::new(
            &id,
            "acct-1/my-app",
            None,
            Some("https://git.example/acct-1/my-app.git"),
            None,
            scope(),
        )
        .expect("build");
        let json = binding.to_json().expect("encode");
        let parsed = Binding::parse(json.as_bytes()).expect("parse");
        assert_eq!(parsed, binding);
        assert_eq!(parsed.channel_id, id.room_id().expect("room"));
        parsed
            .accept_for("https://buzz.example", &"aa".repeat(32))
            .expect("ours and live");
    }

    #[test]
    fn a_document_disagreeing_with_its_own_digest_is_refused() {
        let id = rcs_identity();
        let mut binding = Binding::new(&id, "d", None, None, None, scope()).expect("build");
        binding.binding_id = format!("sha256:{}", "00".repeat(32));
        let json = binding.to_json().expect("encode");
        assert!(Binding::parse(json.as_bytes()).is_err());

        // ...and so is one whose room does not follow from its identity: this
        // is what stops a document pointing a real branch at a stranger's room.
        let mut binding = Binding::new(&id, "d", None, None, None, scope()).expect("build");
        binding.channel_id = "process-".to_string() + &"11".repeat(32);
        let json = binding.to_json().expect("encode");
        assert!(Binding::parse(json.as_bytes()).is_err());
    }

    #[test]
    fn a_foreign_or_archived_binding_is_refused() {
        let id = rcs_identity();
        let mut binding = Binding::new(&id, "d", None, None, None, scope()).expect("build");
        assert!(binding
            .accept_for("https://other.example", &"aa".repeat(32))
            .is_err());
        assert!(binding
            .accept_for("https://buzz.example", &"bb".repeat(32))
            .is_err());
        binding.lifecycle = Lifecycle::Archived;
        assert!(binding
            .accept_for("https://buzz.example", &"aa".repeat(32))
            .is_err());
    }

    #[test]
    fn a_malformed_owner_pubkey_is_refused_at_parse() {
        let id = rcs_identity();
        let mut binding = Binding::new(
            &id,
            "d",
            None,
            None,
            None,
            Scope {
                buzz_origin: "https://buzz.example".into(),
                community_owner_pubkey: "aa".repeat(32),
                visibility: "private".into(),
            },
        )
        .expect("build");
        binding.scope.community_owner_pubkey = "not-hex".into();
        let json = binding.to_json().expect("encode");
        assert!(Binding::parse(json.as_bytes()).is_err());
        binding.scope.community_owner_pubkey = "aa".repeat(32);
        assert!(Binding::parse(binding.to_json().expect("e").as_bytes()).is_ok());
    }

    #[test]
    fn an_unknown_field_is_refused_at_every_level() {
        let binding = Binding::new(&rcs_identity(), "d", None, None, None, scope()).expect("build");
        let json = binding.to_json().expect("encode");
        // Top level.
        let doctored = json.replacen('{', "{\"surprise\":true,", 1);
        assert!(Binding::parse(doctored.as_bytes()).is_err());
        // Inside the repository projection.
        let doctored = json.replace("\"repository\":{", "\"repository\":{\"surprise\":1,");
        assert!(Binding::parse(doctored.as_bytes()).is_err());
        // Inside the scope.
        let doctored = json.replace("\"scope\":{", "\"scope\":{\"surprise\":1,");
        assert!(Binding::parse(doctored.as_bytes()).is_err());
    }

    #[test]
    fn the_document_and_its_projection_strings_are_bounded() {
        let id = rcs_identity();
        // An inflated display name does not change the digest — which is
        // exactly why it has to be bounded here.
        let hostile = |edit: &dyn Fn(&mut Binding)| {
            let mut b = Binding::new(&id, "d", None, None, None, scope()).expect("build");
            edit(&mut b);
            Binding::parse(b.to_json().expect("e").as_bytes()).is_err()
        };
        assert!(hostile(&|b| b.repository.display = "d".repeat(MAX_COMPONENT + 1)));
        // Building never produces such a document: the name is shortened.
        let long = Binding::new(&id, &"d".repeat(MAX_COMPONENT + 9), None, None, None, scope()).expect("build");
        assert!(Binding::parse(long.to_json().expect("e").as_bytes()).is_ok());

        let long_url = format!("https://example.test/{}", "p".repeat(MAX_URL));
        assert!(hostile(&|b| b.repository.clone_url = Some(long_url.clone())));
        assert!(Binding::new(&id, "d", None, Some(&long_url), None, scope()).is_err());

        // An empty display name is not a projection either.
        assert!(hostile(&|b| b.repository.display = String::new()));
        assert!(Binding::new(&id, "", None, None, None, scope()).is_err());

        // A control byte anywhere in the projection.
        assert!(hostile(&|b| b.repository.display = "a\u{1}b".into()));
        assert!(Binding::new(&id, "a\u{1}b", None, None, None, scope()).is_err());

        // A scope owner that is not a pubkey cannot be built.
        let mut bad = scope();
        bad.community_owner_pubkey = "zz".into();
        assert!(Binding::new(&id, "d", None, None, None, bad).is_err());

        // And the whole document.
        let padded = format!("{{\"pad\":\"{}\"}}", "x".repeat(MAX_DOCUMENT));
        assert!(Binding::parse(padded.as_bytes()).is_err());
    }

    #[test]
    fn the_visibility_vocabulary_is_closed() {
        let id = rcs_identity();
        for (visibility, ok) in [("private", true), ("public", true), ("internal", false)] {
            let mut binding = Binding::new(
                &id,
                "d",
                None,
                None,
                None,
                Scope {
                    buzz_origin: "https://buzz.example".into(),
                    community_owner_pubkey: "aa".repeat(32),
                    visibility: "private".into(),
                },
            )
            .expect("build");
            binding.scope.visibility = visibility.into();
            assert_eq!(
                Binding::parse(binding.to_json().expect("e").as_bytes()).is_ok(),
                ok,
                "visibility {visibility}"
            );
        }
    }

    #[test]
    fn the_binding_event_query_is_a_point_read_of_one_event() {
        let q = binding_event_query(&"cc".repeat(32), "process-abc", &"bb".repeat(32));
        assert_eq!(q["ids"], serde_json::json!(["cc".repeat(32)]));
        assert_eq!(q["kinds"], serde_json::json!([9]));
        assert_eq!(q["authors"], serde_json::json!(["bb".repeat(32)]));
        assert_eq!(q["#h"], serde_json::json!(["process-abc"]));
    }

    #[test]
    fn the_event_tags_carry_the_binding_and_room() {
        let binding = Binding::new(&rcs_identity(), "d", None, None, None, scope()).expect("build");
        let tags = binding.tags();
        assert_eq!(tags[0][0], TAG_BINDING);
        assert_eq!(tags[0][1], binding.binding_id);
        assert_eq!(tags[1][1], SCHEMA);
        assert_eq!(tags[2], vec!["h".to_string(), binding.channel_id.clone()]);
    }
}
