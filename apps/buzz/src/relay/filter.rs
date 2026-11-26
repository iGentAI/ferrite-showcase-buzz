//! NIP-01 filter matching — the read-side shape of a subscription.
//!
//! Transliterated from buzz's `filters_match` / `filter_match_one`
//! (`buzz/crates/buzz-core/src/filter.rs`). The semantics are protocol, not
//! preference, and the ones worth stating out loud are:
//!
//! * **OR across filters, AND within one.** A subscription carries a *list* of
//!   filters; an event is delivered if any single filter accepts it, and a
//!   filter accepts only when every field it constrains agrees.
//! * **An empty list matches nothing; an empty filter matches everything.**
//!   Those are different statements. `[]` is "subscribe to nothing" and is what
//!   a closed subscription looks like; `{}` is "subscribe to everything the
//!   access gate will give me".
//! * **`kinds: []` matches nothing.** Not "all kinds". It is the intersection of
//!   an empty set, and a filter that means "nothing" must not silently become a
//!   firehose. Same for an empty tag-value list.
//!
//! Filter matching is *not* an authorization decision. It narrows what a
//! subscriber asked for; [`crate::relay::access::may_deliver`] decides what they are
//! allowed to have, and every delivery path runs both.
//!
//! `search` (NIP-50) IS evaluated here, through the same tokenizer the index is
//! built from ([`crate::relay::search::event_matches`]). The live pump matches
//! with this function, so leaving `search` unevaluated would make a registered
//! `{"search": …}` subscription deliver every authorized future event rather
//! than the matching ones.

use std::collections::BTreeMap;

use ferrite_app::serde::de::{self, IgnoredAny, MapAccess, Visitor};
use ferrite_app::serde::ser::SerializeMap;
use ferrite_app::serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::relay::crypto::to_hex;
use crate::relay::event::Event;
use crate::relay::kind::Kind;

/// A NIP-01 filter.
///
/// `None` means "unconstrained"; `Some(empty)` means "constrained to nothing".
/// [`Default`] is therefore the everything-filter, which is what a subscription
/// with no narrowing looks like on the wire (`{}`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Filter {
    /// Event ids, as lowercase hex. Matched as **prefixes** (NIP-01), so a
    /// caller may supply a truncated id.
    pub ids: Option<Vec<String>>,
    /// Author x-only pubkeys, as lowercase hex. Prefix-matched like `ids`.
    pub authors: Option<Vec<String>>,
    /// Event kinds. Exact match; `Some(vec![])` matches nothing.
    pub kinds: Option<Vec<Kind>>,
    /// Lower bound on `created_at`, **inclusive**.
    pub since: Option<u64>,
    /// Upper bound on `created_at`, **inclusive**.
    pub until: Option<u64>,
    /// Maximum number of stored events to return. Advisory to the query front;
    /// not part of matching.
    pub limit: Option<u32>,
    /// NIP-50 search term. The stored-read path routes it to the term index for
    /// candidate selection; [`Filter::matches`] also evaluates it, through the
    /// same tokenizer, so live delivery agrees with history.
    pub search: Option<String>,
    /// Single-letter tag constraints, keyed by their **wire name including the
    /// `#`** (`"#h"`, `"#e"`, `"#p"`). A `BTreeMap` so serialization is
    /// deterministic: filters are stored inside session documents that are
    /// compare-and-swapped on their exact bytes.
    pub tags: BTreeMap<String, Vec<String>>,
}

impl Filter {
    /// The values constrained for a tag, whether the caller names it `"h"` or
    /// `"#h"`.
    ///
    /// The gate in [`crate::relay::access::filter_authorized`] needs the `#p` list to
    /// decide whether a filter may touch `#p`-gated kinds at all; going through
    /// this accessor keeps the caller from having to know how the key is spelled.
    #[must_use]
    pub fn tag_filter(&self, name: &str) -> Option<&[String]> {
        let bare = name.strip_prefix('#').unwrap_or(name);
        let mut key = String::with_capacity(bare.len() + 1);
        key.push('#');
        key.push_str(bare);
        self.tags.get(&key).map(Vec::as_slice)
    }

    /// Whether this is a NIP-50 search filter, which the query front must route
    /// to the search engine rather than resolve from index keys.
    #[must_use]
    pub const fn is_search(&self) -> bool {
        self.search.is_some()
    }

    /// Does `event` satisfy every constraint in this filter?
    ///
    /// `channel_id` is the channel ingest *resolved* for the event, which is not
    /// always the channel the author tagged: reactions (kind 7) and deletions
    /// (kind 5) inherit their channel from the event they target and carry no
    /// `h` tag of their own. It is consulted only as a fallback, and only when
    /// the event carries no `h` tag at all.
    ///
    /// That asymmetry is a security property, not an optimization. An explicit
    /// `h` tag is **authoritative**: if the event names channel B and the filter
    /// asks for channel A, the answer is no — even when the resolved channel is
    /// A. Letting the fallback override a stated tag would let an event
    /// cross-post itself into a channel a subscriber is watching.
    #[must_use]
    pub fn matches(&self, event: &Event, channel_id: Option<&str>) -> bool {
        // Kinds first: it is the cheapest test and the one that eliminates most
        // events on a busy relay.
        if let Some(kinds) = &self.kinds {
            if !kinds.contains(&event.kind) {
                return false;
            }
        }

        if let Some(ids) = &self.ids {
            if !hex_prefix_matches(&to_hex(&event.id), ids) {
                return false;
            }
        }

        if let Some(authors) = &self.authors {
            if !hex_prefix_matches(&to_hex(&event.pubkey), authors) {
                return false;
            }
        }

        // Both bounds are inclusive: `since == until == created_at` matches the
        // single event at that second.
        if let Some(since) = self.since {
            if event.created_at < since {
                return false;
            }
        }
        if let Some(until) = self.until {
            if event.created_at > until {
                return false;
            }
        }

        // NIP-50. Evaluated through the SAME tokenizer the index is built from,
        // so a live delivery and a historical query can never disagree about
        // whether an event matches a term. The stored path still uses the index
        // to CHOOSE candidates — this only confirms them, and is the only check
        // the live pump has.
        if let Some(query) = &self.search {
            if !crate::relay::search::event_matches(query, event) {
                return false;
            }
        }

        for (key, wanted) in &self.tags {
            let name = key.strip_prefix('#').unwrap_or(key.as_str());
            if event
                .tag_values(name)
                .any(|v| wanted.iter().any(|w| w == v))
            {
                continue;
            }
            // No tag on the event matched. For `#h` only, an event that
            // asserted no `h` tag at all may still match via the channel ingest
            // resolved for it. All three conjuncts are required: the name, the
            // absence of any author-stated `h`, and a resolved channel the
            // filter actually asked for.
            let h_fallback_applies = name == "h"
                && !event.has_tag("h")
                && channel_id.is_some_and(|c| wanted.iter().any(|w| w == c));
            if h_fallback_applies {
                continue;
            }
            return false;
        }

        true
    }
}

/// Prefix-match a lowercase-hex value against a list of (possibly truncated)
/// hex prefixes.
///
/// Comparison is ASCII-case-insensitive because the value side is always
/// re-rendered from bytes: a client that upcases its hex is asking for the same
/// event. A prefix longer than the value never matches. An *empty* prefix
/// matches everything, which is the `starts_with` semantics buzz has and is
/// harmless: `ids`/`authors` only ever narrow a subscription, and authorization
/// is decided separately by [`crate::relay::access`].
fn hex_prefix_matches(value: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| {
        value
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    })
}

/// The kind allowlist a set of NIP-01 filters implies, for the read that will
/// answer them.
///
/// NIP-01 ORs across the filter array and ANDs within one filter, so an event
/// can only be part of the answer if **some** filter admits its kind. When every
/// filter constrains `kinds`, their union is therefore a sound pre-filter: an
/// event outside it cannot match any filter, so declining to read it into the
/// page changes no answer — it only stops the reply from carrying bytes the
/// caller already said it did not want.
///
/// Returns [`crate::relay::query::KindSelect::all`] whenever that reasoning does not
/// hold:
///
/// * no filters at all — nothing was narrowed;
/// * any filter leaves `kinds` unconstrained — that filter admits every kind,
///   so the union is unbounded;
/// * the union is wider than [`crate::relay::query::MAX_SELECT_KINDS`] — past that the
///   allowlist scan costs more than the rows it would reject.
///
/// This is a **pre-filter, not the filter**. Every other constraint (authors,
/// tags, `since`/`until`, id prefixes) is still evaluated by [`Filter::matches`]
/// on the events that come back, so narrowing here can only remove events
/// [`Filter::matches`] was going to reject anyway.
///
/// The motivating case: `QueryRequest` (buzz-sdk `transport.rs`) already carries
/// `filters` in the `POST /buzz/api/query` body and already re-applies them
/// client-side — its own doc says the narrowing is honored "whether or not the
/// relay evaluates filters itself" — so today a narrowed SDK query pays for the
/// whole channel and discards most of it. Honoring the kinds the caller already
/// sent is not a wire change.
#[must_use]
pub fn kind_selector(filters: &[Filter]) -> crate::relay::query::KindSelect {
    use crate::relay::query::{KindSelect, MAX_SELECT_KINDS};

    if filters.is_empty() {
        return KindSelect::all();
    }
    let mut union: Vec<Kind> = Vec::new();
    for filter in filters {
        // An unconstrained `kinds` admits everything, so the union is unbounded
        // and no pre-filter is sound. (`Some(vec![])` is NOT this case: it
        // matches nothing, and contributes nothing to the union.)
        let Some(kinds) = filter.kinds.as_deref() else {
            return KindSelect::all();
        };
        for k in kinds {
            if !union.contains(k) {
                if union.len() >= MAX_SELECT_KINDS {
                    return KindSelect::all();
                }
                union.push(*k);
            }
        }
    }
    KindSelect::only(union)
}

/// Does `event` satisfy **any** of `filters`?
///
/// An empty slice matches nothing — a subscription that constrained itself to no
/// filters receives no events, which is what closing a subscription looks like
/// to the delivery pump.
#[must_use]
pub fn filters_match(filters: &[Filter], event: &Event, channel_id: Option<&str>) -> bool {
    filters.iter().any(|f| f.matches(event, channel_id))
}

/// Serializes to the NIP-01 wire shape: absent fields are omitted (they mean
/// "unconstrained", which is not the same as an empty list), and tag
/// constraints are emitted under their `#x` keys.
impl Serialize for Filter {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        let present = usize::from(self.ids.is_some())
            + usize::from(self.authors.is_some())
            + usize::from(self.kinds.is_some())
            + usize::from(self.since.is_some())
            + usize::from(self.until.is_some())
            + usize::from(self.limit.is_some())
            + usize::from(self.search.is_some())
            + self.tags.len();
        let mut map = serializer.serialize_map(Some(present))?;
        if let Some(ids) = &self.ids {
            map.serialize_entry("ids", ids)?;
        }
        if let Some(authors) = &self.authors {
            map.serialize_entry("authors", authors)?;
        }
        if let Some(kinds) = &self.kinds {
            map.serialize_entry("kinds", kinds)?;
        }
        if let Some(since) = &self.since {
            map.serialize_entry("since", since)?;
        }
        if let Some(until) = &self.until {
            map.serialize_entry("until", until)?;
        }
        if let Some(limit) = &self.limit {
            map.serialize_entry("limit", limit)?;
        }
        if let Some(search) = &self.search {
            map.serialize_entry("search", search)?;
        }
        // BTreeMap iteration is sorted, so the bytes are reproducible.
        for (key, values) in &self.tags {
            map.serialize_entry(key, values)?;
        }
        map.end()
    }
}

/// Deserializes the NIP-01 wire shape.
///
/// Hand-rolled because the `#x` tag keys are open-ended: no derive can express
/// "collect every key beginning with `#` into a map". Keys that are neither a
/// known field nor `#`-prefixed are ignored, which is what NIP-01 asks a relay
/// to do with a filter field it does not implement.
impl<'de> Deserialize<'de> for Filter {
    fn deserialize<D: Deserializer<'de>>(d: D) -> core::result::Result<Self, D::Error> {
        struct FilterVisitor;

        impl<'de> Visitor<'de> for FilterVisitor {
            type Value = Filter;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("a NIP-01 filter object")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> core::result::Result<Filter, A::Error> {
                let mut out = Filter::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "ids" => set_once(&mut out.ids, &mut map, "ids")?,
                        "authors" => set_once(&mut out.authors, &mut map, "authors")?,
                        "kinds" => set_once(&mut out.kinds, &mut map, "kinds")?,
                        "since" => set_once(&mut out.since, &mut map, "since")?,
                        "until" => set_once(&mut out.until, &mut map, "until")?,
                        "limit" => set_once(&mut out.limit, &mut map, "limit")?,
                        "search" => set_once(&mut out.search, &mut map, "search")?,
                        _ if key.starts_with('#') => {
                            let values = map.next_value::<Vec<String>>()?;
                            if out.tags.insert(key, values).is_some() {
                                return Err(de::Error::custom("duplicate tag filter key"));
                            }
                        }
                        // Unknown field: NIP-01 relays ignore what they do not
                        // implement rather than failing the subscription.
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(out)
            }
        }

        d.deserialize_map(FilterVisitor)
    }
}

/// Read one filter field, refusing a repeated key.
///
/// A filter with `kinds` twice is ambiguous, and the ambiguity is exploitable:
/// an authorization pre-check reading one occurrence and the matcher reading the
/// other is exactly the shape of a gate bypass.
fn set_once<'de, T, A>(
    slot: &mut Option<T>,
    map: &mut A,
    field: &'static str,
) -> core::result::Result<(), A::Error>
where
    T: Deserialize<'de>,
    A: MapAccess<'de>,
{
    if slot.is_some() {
        return Err(de::Error::duplicate_field(field));
    }
    *slot = Some(map.next_value()?);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use ferrite_app::serde_json;

    const KIND_NOTE: Kind = 1;
    const KIND_REACTION: Kind = 7;
    const KIND_DELETION: Kind = 5;

    fn event(kind: Kind, id: u8, author: u8, created_at: u64, tags: Vec<Vec<String>>) -> Event {
        Event {
            id: [id; 32],
            pubkey: [author; 32],
            created_at,
            kind,
            tags,
            content: "body".into(),
            sig: [0u8; 64],
        }
    }

    fn note() -> Event {
        event(KIND_NOTE, 0xab, 0xcd, 1_000, vec![])
    }

    fn tag(name: &str, value: &str) -> Vec<String> {
        vec![name.to_string(), value.to_string()]
    }

    fn tag_filter_of(pairs: &[(&str, &[&str])]) -> Filter {
        let mut f = Filter::default();
        for (key, values) in pairs {
            f.tags.insert(
                (*key).to_string(),
                values.iter().map(|v| (*v).to_string()).collect(),
            );
        }
        f
    }

    #[test]
    fn a_default_filter_matches_everything() {
        let f = Filter::default();
        assert!(f.matches(&note(), None));
        assert!(f.matches(&note(), Some("chan-1")));
        assert!(f.matches(&event(30023, 1, 2, 0, vec![tag("d", "x")]), None));
        assert!(f.matches(&event(u32::MAX, 9, 9, u64::MAX, vec![]), None));
    }

    #[test]
    fn an_empty_filter_list_matches_nothing() {
        assert!(!filters_match(&[], &note(), None));
        assert!(!filters_match(&[], &note(), Some("chan-1")));
        assert!(filters_match(&[Filter::default()], &note(), None));
    }

    #[test]
    fn empty_kinds_matches_nothing_and_absent_kinds_matches_all() {
        let nothing = Filter {
            kinds: Some(vec![]),
            ..Filter::default()
        };
        assert!(!nothing.matches(&note(), None));
        assert!(!nothing.matches(&event(KIND_REACTION, 1, 1, 1, vec![]), None));

        let all = Filter {
            kinds: None,
            ..Filter::default()
        };
        assert!(all.matches(&note(), None));
        assert!(all.matches(&event(KIND_REACTION, 1, 1, 1, vec![]), None));
    }

    #[test]
    fn an_empty_tag_value_list_matches_nothing() {
        // Same reasoning as empty kinds: the intersection of nothing is nothing,
        // and a filter that means "no channel" must not become "every channel".
        let f = tag_filter_of(&[("#h", &[])]);
        assert!(!f.matches(&event(KIND_NOTE, 1, 1, 1, vec![tag("h", "chan-1")]), None));
        assert!(!f.matches(&note(), Some("chan-1")));
    }

    #[test]
    fn kinds_match_exactly_never_by_range() {
        let f = Filter {
            kinds: Some(vec![KIND_NOTE, 30023]),
            ..Filter::default()
        };
        assert!(f.matches(&event(KIND_NOTE, 1, 1, 1, vec![]), None));
        assert!(f.matches(&event(30023, 1, 1, 1, vec![]), None));
        assert!(!f.matches(&event(30024, 1, 1, 1, vec![]), None));
        assert!(!f.matches(&event(0, 1, 1, 1, vec![]), None));
    }

    #[test]
    fn ids_match_by_full_value_and_by_prefix() {
        let e = note();
        let full = to_hex(&e.id);

        for prefix in [full.as_str(), &full[..2], &full[..7], &full[..63]] {
            let f = Filter {
                ids: Some(vec![prefix.to_string()]),
                ..Filter::default()
            };
            assert!(f.matches(&e, None), "prefix {prefix:?} did not match");
        }

        let wrong = Filter {
            ids: Some(vec!["ac".into()]),
            ..Filter::default()
        };
        assert!(!wrong.matches(&e, None));
    }

    #[test]
    fn authors_match_by_full_value_and_by_prefix() {
        let e = note();
        let full = to_hex(&e.pubkey);

        let partial = Filter {
            authors: Some(vec![full[..5].to_string()]),
            ..Filter::default()
        };
        assert!(partial.matches(&e, None));

        let exact = Filter {
            authors: Some(vec![full.clone()]),
            ..Filter::default()
        };
        assert!(exact.matches(&e, None));

        let other = Filter {
            authors: Some(vec![to_hex(&[0x11u8; 32])]),
            ..Filter::default()
        };
        assert!(!other.matches(&e, None));
    }

    #[test]
    fn a_prefix_longer_than_the_value_never_matches() {
        let e = note();
        let f = Filter {
            ids: Some(vec![format!("{}00", to_hex(&e.id))]),
            ..Filter::default()
        };
        assert!(!f.matches(&e, None));
    }

    #[test]
    fn prefix_matching_ignores_hex_case() {
        // The value side is always re-rendered from bytes, so an upcasing client
        // is asking for the same event.
        let e = note();
        let f = Filter {
            ids: Some(vec![to_hex(&e.id).to_uppercase()]),
            ..Filter::default()
        };
        assert!(f.matches(&e, None));
    }

    #[test]
    fn non_hex_and_empty_prefixes_behave_predictably() {
        let e = note();
        let garbage = Filter {
            ids: Some(vec!["zzzz".into()]),
            ..Filter::default()
        };
        assert!(!garbage.matches(&e, None));

        // `starts_with` semantics: an empty prefix constrains nothing. Harmless
        // because ids only narrow, and access is decided elsewhere.
        let empty = Filter {
            ids: Some(vec![String::new()]),
            ..Filter::default()
        };
        assert!(empty.matches(&e, None));
    }

    #[test]
    fn any_listed_id_or_author_is_enough() {
        let e = note();
        let f = Filter {
            ids: Some(vec!["00".into(), to_hex(&e.id)]),
            authors: Some(vec!["11".into(), to_hex(&e.pubkey)]),
            ..Filter::default()
        };
        assert!(f.matches(&e, None));
    }

    #[test]
    fn since_and_until_are_inclusive_at_the_exact_boundary() {
        let e = event(KIND_NOTE, 1, 1, 1_000, vec![]);

        let at_since = Filter {
            since: Some(1_000),
            ..Filter::default()
        };
        assert!(at_since.matches(&e, None), "since == created_at must match");

        let past_since = Filter {
            since: Some(1_001),
            ..Filter::default()
        };
        assert!(!past_since.matches(&e, None));

        let at_until = Filter {
            until: Some(1_000),
            ..Filter::default()
        };
        assert!(at_until.matches(&e, None), "until == created_at must match");

        let before_until = Filter {
            until: Some(999),
            ..Filter::default()
        };
        assert!(!before_until.matches(&e, None));

        // A single-second window pinned exactly on the event.
        let pinned = Filter {
            since: Some(1_000),
            until: Some(1_000),
            ..Filter::default()
        };
        assert!(pinned.matches(&e, None));

        // An inverted window matches nothing.
        let inverted = Filter {
            since: Some(1_001),
            until: Some(999),
            ..Filter::default()
        };
        assert!(!inverted.matches(&e, None));
    }

    #[test]
    fn fields_within_one_filter_are_anded() {
        let e = event(KIND_NOTE, 0xab, 0xcd, 1_000, vec![]);
        let hit = Filter {
            kinds: Some(vec![KIND_NOTE]),
            authors: Some(vec![to_hex(&e.pubkey)]),
            since: Some(999),
            until: Some(1_001),
            ..Filter::default()
        };
        assert!(hit.matches(&e, None));

        // Flip one conjunct at a time; each alone must reject.
        let wrong_kind = Filter {
            kinds: Some(vec![KIND_REACTION]),
            ..hit.clone()
        };
        assert!(!wrong_kind.matches(&e, None));
        let wrong_author = Filter {
            authors: Some(vec![to_hex(&[0x01u8; 32])]),
            ..hit.clone()
        };
        assert!(!wrong_author.matches(&e, None));
        let wrong_window = Filter {
            since: Some(2_000),
            ..hit
        };
        assert!(!wrong_window.matches(&e, None));
    }

    #[test]
    fn filters_are_ored_across_the_list() {
        let e = note();
        let miss = Filter {
            kinds: Some(vec![KIND_REACTION]),
            ..Filter::default()
        };
        let hit = Filter {
            kinds: Some(vec![KIND_NOTE]),
            ..Filter::default()
        };
        assert!(filters_match(&[miss.clone(), hit.clone()], &e, None));
        assert!(filters_match(&[hit, miss.clone()], &e, None));

        let other_miss = Filter {
            kinds: Some(vec![KIND_DELETION]),
            ..Filter::default()
        };
        assert!(!filters_match(&[miss, other_miss], &e, None));
    }

    #[test]
    fn or_across_filters_does_not_merge_their_conjuncts() {
        // Guard against an implementation that unions fields instead of
        // filters: kind 1 from author A, or kind 7 from author B — an event that
        // is kind 1 from author B must not match.
        let a = to_hex(&[0xaau8; 32]);
        let b = to_hex(&[0xbbu8; 32]);
        let filters = vec![
            Filter {
                kinds: Some(vec![KIND_NOTE]),
                authors: Some(vec![a]),
                ..Filter::default()
            },
            Filter {
                kinds: Some(vec![KIND_REACTION]),
                authors: Some(vec![b]),
                ..Filter::default()
            },
        ];
        assert!(filters_match(
            &filters,
            &event(KIND_NOTE, 1, 0xaa, 1, vec![]),
            None
        ));
        assert!(filters_match(
            &filters,
            &event(KIND_REACTION, 1, 0xbb, 1, vec![]),
            None
        ));
        assert!(!filters_match(
            &filters,
            &event(KIND_NOTE, 1, 0xbb, 1, vec![]),
            None
        ));
        assert!(!filters_match(
            &filters,
            &event(KIND_REACTION, 1, 0xaa, 1, vec![]),
            None
        ));
    }

    #[test]
    fn a_tag_filter_matches_the_second_element_of_a_named_tag() {
        let e = event(
            KIND_NOTE,
            1,
            1,
            1,
            vec![tag("e", &"a".repeat(64)), tag("p", &"b".repeat(64))],
        );
        assert!(tag_filter_of(&[("#e", &[&"a".repeat(64)])]).matches(&e, None));
        assert!(!tag_filter_of(&[("#e", &[&"c".repeat(64)])]).matches(&e, None));
        // Any listed value is enough.
        assert!(tag_filter_of(&[("#e", &[&"c".repeat(64), &"a".repeat(64)])]).matches(&e, None));
    }

    #[test]
    fn two_different_tag_names_are_anded() {
        let target = "a".repeat(64);
        let who = "b".repeat(64);
        let e = event(KIND_NOTE, 1, 1, 1, vec![tag("e", &target), tag("p", &who)]);

        assert!(tag_filter_of(&[("#e", &[&target]), ("#p", &[&who])]).matches(&e, None));
        // Satisfying only one of the two names is not enough.
        assert!(
            !tag_filter_of(&[("#e", &[&target]), ("#p", &[&"c".repeat(64)])]).matches(&e, None)
        );
        assert!(!tag_filter_of(&[("#e", &[&"c".repeat(64)]), ("#p", &[&who])]).matches(&e, None));
        // A name the event does not carry at all fails the conjunct.
        assert!(!tag_filter_of(&[("#e", &[&target]), ("#t", &["topic"])]).matches(&e, None));
    }

    #[test]
    fn tag_values_are_matched_exactly_not_by_prefix() {
        let e = event(KIND_NOTE, 1, 1, 1, vec![tag("t", "release")]);
        assert!(tag_filter_of(&[("#t", &["release"])]).matches(&e, None));
        assert!(!tag_filter_of(&[("#t", &["rel"])]).matches(&e, None));
        assert!(!tag_filter_of(&[("#t", &["Release"])]).matches(&e, None));
    }

    #[test]
    fn a_valueless_tag_on_the_event_satisfies_nothing() {
        let e = event(KIND_NOTE, 1, 1, 1, vec![vec!["t".into()]]);
        assert!(!tag_filter_of(&[("#t", &[""])]).matches(&e, None));
        assert!(!tag_filter_of(&[("#t", &["x"])]).matches(&e, None));
    }

    #[test]
    fn h_falls_back_to_the_resolved_channel_when_the_event_has_no_h_tag() {
        // Reactions and deletions inherit their channel from their target and
        // carry no h tag; without the fallback they would never reach a
        // channel-scoped subscription.
        let reaction = event(KIND_REACTION, 1, 1, 1, vec![tag("e", &"a".repeat(64))]);
        let f = tag_filter_of(&[("#h", &["chan-1"])]);

        assert!(f.matches(&reaction, Some("chan-1")), "fallback must apply");
        assert!(!f.matches(&reaction, Some("chan-2")), "wrong channel");
        assert!(!f.matches(&reaction, None), "no resolved channel, no match");
    }

    #[test]
    fn an_explicit_h_tag_is_authoritative_and_the_fallback_cannot_override_it() {
        // The cross-channel leak this rule exists to prevent: the event was
        // resolved into chan-1 (which the subscriber watches) but its own h tag
        // names chan-2. The stated tag wins and the event is withheld.
        let e = event(KIND_NOTE, 1, 1, 1, vec![tag("h", "chan-2")]);
        let f = tag_filter_of(&[("#h", &["chan-1"])]);
        assert!(
            !f.matches(&e, Some("chan-1")),
            "an explicit h tag must not be overridden by the resolved channel"
        );

        // And the ordinary direction: the stated tag matches, so the resolved
        // channel is never consulted.
        let ok = tag_filter_of(&[("#h", &["chan-2"])]);
        assert!(ok.matches(&e, Some("chan-1")));
        assert!(ok.matches(&e, None));
    }

    #[test]
    fn a_valueless_h_tag_still_suppresses_the_fallback() {
        // `["h"]` is an assertion by the author that this event's channel is
        // stated, even though it states nothing. Treating it as "no h tag" would
        // reopen the override the previous test closes.
        let e = event(KIND_NOTE, 1, 1, 1, vec![vec!["h".into()]]);
        let f = tag_filter_of(&[("#h", &["chan-1"])]);
        assert!(!f.matches(&e, Some("chan-1")));
    }

    #[test]
    fn the_fallback_applies_only_to_h_not_to_other_tag_names() {
        let e = event(KIND_REACTION, 1, 1, 1, vec![]);
        // `#e` gets no channel fallback, so an event with no e tag never matches.
        assert!(!tag_filter_of(&[("#e", &["chan-1"])]).matches(&e, Some("chan-1")));
        assert!(!tag_filter_of(&[("#p", &["chan-1"])]).matches(&e, Some("chan-1")));
    }

    #[test]
    fn a_second_h_tag_does_not_widen_the_match() {
        // Both h tags are the event's own, so both are authoritative and either
        // may satisfy the filter — but an unrelated channel still cannot.
        let e = event(
            KIND_NOTE,
            1,
            1,
            1,
            vec![tag("h", "chan-1"), tag("h", "chan-2")],
        );
        assert!(tag_filter_of(&[("#h", &["chan-1"])]).matches(&e, None));
        assert!(tag_filter_of(&[("#h", &["chan-2"])]).matches(&e, None));
        assert!(!tag_filter_of(&[("#h", &["chan-3"])]).matches(&e, Some("chan-3")));
    }

    #[test]
    fn tag_filter_keys_may_be_named_with_or_without_the_hash() {
        let f = tag_filter_of(&[("#p", &["abc"])]);
        assert_eq!(f.tag_filter("p"), Some(&["abc".to_string()][..]));
        assert_eq!(f.tag_filter("#p"), Some(&["abc".to_string()][..]));
        assert_eq!(f.tag_filter("e"), None);
    }

    #[test]
    fn search_is_reported_for_routing_and_also_applied_by_matches() {
        let f = Filter {
            search: Some("nothing in this event".into()),
            ..Filter::default()
        };
        assert!(f.is_search());
        assert!(!Filter::default().is_search());
        // A term the event does not carry must NOT match. This is the live
        // subscription case: the pump matches with this function, so ignoring
        // `search` here turned every NIP-50 subscription into a firehose.
        assert!(!f.matches(&note(), None));

        // And a term it does carry matches — through the index's own tokenizer,
        // so live delivery and historical search agree. `note()` is kind 1,
        // which is not a searchable kind, so a searchable one is used here.
        let mut message = note();
        message.kind = crate::relay::kind::KIND_STREAM_MESSAGE;
        message.content = "deploy the relay".into();
        assert!(Filter {
            search: Some("RELAY deploy".into()),
            ..Filter::default()
        }
        .matches(&message, None));

        // Every other conjunct still applies alongside it.
        assert!(!Filter {
            search: Some("deploy".into()),
            kinds: Some(vec![KIND_REACTION]),
            ..Filter::default()
        }
        .matches(&message, None));
    }

    #[test]
    fn serde_round_trips_a_filter_carrying_h_and_e_tags() {
        let f = Filter {
            ids: Some(vec!["ab".into()]),
            authors: Some(vec!["cd".into()]),
            kinds: Some(vec![KIND_NOTE, KIND_REACTION]),
            since: Some(10),
            until: Some(20),
            limit: Some(50),
            search: Some("term".into()),
            tags: [
                ("#h".to_string(), vec!["chan-1".to_string()]),
                ("#e".to_string(), vec!["a".repeat(64), "b".repeat(64)]),
            ]
            .into_iter()
            .collect(),
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"#h\":[\"chan-1\"]"), "{json}");
        assert!(json.contains("\"#e\":["), "{json}");
        assert_eq!(serde_json::from_str::<Filter>(&json).unwrap(), f);
    }

    #[test]
    fn absent_fields_stay_absent_on_the_wire() {
        assert_eq!(serde_json::to_string(&Filter::default()).unwrap(), "{}");
        assert_eq!(
            serde_json::from_str::<Filter>("{}").unwrap(),
            Filter::default()
        );
    }

    #[test]
    fn an_empty_list_is_distinguishable_from_an_absent_field_on_the_wire() {
        // The two mean opposite things, so the encoding must keep them apart.
        let empty = Filter {
            kinds: Some(vec![]),
            ..Filter::default()
        };
        let json = serde_json::to_string(&empty).unwrap();
        assert_eq!(json, "{\"kinds\":[]}");
        assert_eq!(serde_json::from_str::<Filter>(&json).unwrap(), empty);
        assert_ne!(serde_json::from_str::<Filter>("{}").unwrap(), empty);
    }

    #[test]
    fn unknown_filter_fields_are_ignored() {
        let f: Filter = serde_json::from_str(
            "{\"kinds\":[1],\"unknown\":\"x\",\"nested\":{\"a\":[1,2]},\"limit\":3}",
        )
        .unwrap();
        assert_eq!(f.kinds, Some(vec![1]));
        assert_eq!(f.limit, Some(3));
        assert!(f.tags.is_empty());
    }

    #[test]
    fn arbitrary_hash_keys_are_collected_as_tag_filters() {
        let f: Filter =
            serde_json::from_str("{\"#h\":[\"c\"],\"#zz\":[\"v\"],\"#\":[\"bare\"]}").unwrap();
        assert_eq!(f.tags.len(), 3);
        assert_eq!(f.tag_filter("h"), Some(&["c".to_string()][..]));
        assert_eq!(f.tags.get("#zz"), Some(&vec!["v".to_string()]));
        // A bare "#" names the empty tag, which no well-formed event carries.
        assert!(!f.matches(&note(), Some("c")));
    }

    #[test]
    fn a_multi_letter_tag_filter_matches_the_same_way() {
        let e = event(KIND_NOTE, 1, 1, 1, vec![tag("expiration", "999")]);
        let f: Filter = serde_json::from_str("{\"#expiration\":[\"999\"]}").unwrap();
        assert!(f.matches(&e, None));
    }

    #[test]
    fn malformed_filters_are_refused() {
        assert!(serde_json::from_str::<Filter>("[]").is_err());
        assert!(serde_json::from_str::<Filter>("{\"kinds\":1}").is_err());
        assert!(serde_json::from_str::<Filter>("{\"kinds\":[-1]}").is_err());
        assert!(serde_json::from_str::<Filter>("{\"kinds\":[4294967296]}").is_err());
        assert!(serde_json::from_str::<Filter>("{\"since\":\"10\"}").is_err());
        assert!(serde_json::from_str::<Filter>("{\"limit\":-1}").is_err());
        assert!(serde_json::from_str::<Filter>("{\"ids\":\"ab\"}").is_err());
        assert!(serde_json::from_str::<Filter>("{\"#h\":\"chan\"}").is_err());
        assert!(serde_json::from_str::<Filter>("{\"#h\":[1]}").is_err());
    }

    #[test]
    fn a_repeated_filter_field_is_refused_rather_than_last_wins() {
        // An authorization pre-check reading one occurrence while the matcher
        // reads the other is exactly the shape of a gate bypass.
        assert!(serde_json::from_str::<Filter>("{\"kinds\":[1],\"kinds\":[1059]}").is_err());
        assert!(serde_json::from_str::<Filter>("{\"#p\":[\"mine\"],\"#p\":[\"theirs\"]}").is_err());
        assert!(serde_json::from_str::<Filter>("{\"limit\":1,\"limit\":2}").is_err());
    }

    #[test]
    fn serialization_is_byte_stable_across_equal_filters() {
        // Filters live inside session documents that are compare-and-swapped on
        // their exact bytes, so re-encoding an unchanged filter must reproduce
        // them. The BTreeMap is what makes the tag order deterministic.
        let build = || {
            let mut f = Filter {
                kinds: Some(vec![KIND_NOTE]),
                ..Filter::default()
            };
            // Insert in the opposite order the second time round.
            f.tags.insert("#z".into(), vec!["1".into()]);
            f.tags.insert("#a".into(), vec!["2".into()]);
            f
        };
        let mut other = Filter {
            kinds: Some(vec![KIND_NOTE]),
            ..Filter::default()
        };
        other.tags.insert("#a".into(), vec!["2".into()]);
        other.tags.insert("#z".into(), vec!["1".into()]);

        let a = serde_json::to_string(&build()).unwrap();
        let b = serde_json::to_string(&other).unwrap();
        assert_eq!(a, b);
        assert!(a.find("\"#a\"").unwrap() < a.find("\"#z\"").unwrap());
    }

    fn of_kinds(kinds: Option<Vec<Kind>>) -> Filter {
        Filter {
            kinds,
            ..Filter::default()
        }
    }

    #[test]
    fn a_narrowed_filter_set_yields_its_kind_union() {
        let select = kind_selector(&[
            of_kinds(Some(vec![KIND_NOTE])),
            of_kinds(Some(vec![KIND_REACTION, KIND_NOTE])),
        ]);
        assert!(select.admits(KIND_NOTE));
        assert!(select.admits(KIND_REACTION));
        assert!(!select.admits(KIND_DELETION));
        assert!(!select.admits(39002));
    }

    #[test]
    fn an_unnarrowed_filter_set_yields_no_pre_filter() {
        // No filters at all: nothing was narrowed.
        assert!(kind_selector(&[]).is_unrestricted());
        // A filter with no `kinds` admits every kind, so the union is unbounded
        // — even when a SIBLING filter is narrow. Getting this wrong would drop
        // events the open filter was entitled to.
        assert!(kind_selector(&[of_kinds(None)]).is_unrestricted());
        assert!(
            kind_selector(&[of_kinds(Some(vec![KIND_NOTE])), of_kinds(None)]).is_unrestricted()
        );
        // A union too wide to scan cheaply also falls back to no pre-filter.
        let wide: Vec<Filter> = (0..64).map(|k| of_kinds(Some(vec![k]))).collect();
        assert!(kind_selector(&wide).is_unrestricted());
    }

    #[test]
    fn an_empty_kinds_list_contributes_nothing() {
        // `Some(vec![])` matches nothing (see `Filter::matches`), so it must not
        // be confused with "unconstrained" — it widens the union by zero.
        let select = kind_selector(&[of_kinds(Some(vec![])), of_kinds(Some(vec![KIND_NOTE]))]);
        assert!(select.admits(KIND_NOTE));
        assert!(!select.admits(KIND_REACTION));
        // All-empty: the filters match nothing, so the pre-filter admits nothing.
        let none = kind_selector(&[of_kinds(Some(vec![]))]);
        assert!(!none.is_unrestricted());
        assert!(!none.admits(KIND_NOTE));
    }

    /// **The soundness property.** The pre-filter may never reject an event that
    /// the filters themselves would have matched — otherwise narrowing the read
    /// would silently change the answer instead of only making it cheaper.
    #[test]
    fn the_pre_filter_never_rejects_an_event_the_filters_match() {
        let corpus: Vec<Event> = [1u32, 5, 7, 9, 40002, 39002, 9021, 43001]
            .iter()
            .enumerate()
            .map(|(i, k)| {
                event(
                    *k,
                    u8::try_from(i).unwrap_or(0),
                    0xcd,
                    1_000,
                    vec![tag("h", "general")],
                )
            })
            .collect();

        let filter_sets: Vec<Vec<Filter>> = vec![
            vec![],
            vec![of_kinds(None)],
            vec![of_kinds(Some(vec![9]))],
            vec![of_kinds(Some(vec![9, 7]))],
            vec![of_kinds(Some(vec![9])), of_kinds(Some(vec![39002]))],
            vec![of_kinds(Some(vec![9])), of_kinds(None)],
            vec![of_kinds(Some(vec![]))],
            vec![Filter {
                kinds: Some(vec![9]),
                since: Some(2_000),
                ..Filter::default()
            }],
        ];

        for filters in &filter_sets {
            let select = kind_selector(filters);
            for ev in &corpus {
                if filters_match(filters, ev, Some("general")) {
                    assert!(
                        select.admits(ev.kind),
                        "pre-filter rejected kind {} that the filters match",
                        ev.kind
                    );
                }
            }
        }
    }
}
