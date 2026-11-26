//! Work the app does once at boot: seeding, feed topics and the reconcilers that
//! repair projections after a crash.

use ferrite_app::tracing;
use ferrite_app::serde_json;
use ferrite_app::AppContext;
use crate::relay::event::Event;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use crate::relay::DEFAULT_COMMUNITY;
use super::{FEED_GENERATION};
use super::events::{ingest_event};
use super::agents::{MAX_CLIENT_AGENTS, registry_or_empty};
use super::identity::{relay_identity};

/// Whether boot copies the embedded web client into the `web-assets` blob (see the call site): off, because
/// upstream's gateway never serves that blob while the api `/` route exists.
pub(super) const SEED_UI_BLOB: bool = false;

/// Seed the embedded web client into the `web-assets` blob under `ui/`.
///
/// The embedded bundle is a FIRST-BOOT BOOTSTRAP only: once an operator has
/// pushed a real bundle through the assets routes, its manifest
/// (`buzz-web-assets-manifest`) is the authority and this function does
/// nothing — before that watermark existed, every boot rewrote any `ui/`
/// object that differed from the embedded bytes, silently clobbering a pushed
/// bundle. On a manifest READ error, seeding is skipped entirely: clobbering a
/// pushed bundle is strictly worse than one boot without seeding, and the next
/// boot heals.
///
/// Write-if-changed: a redeploy of an unchanged bundle costs one read per
/// object instead of a rewrite, and a partially-seeded set self-heals on the
/// next boot. Failures are logged, never fatal — the API face does not depend
/// on the UI being present. `pub` so the anti-clobber gate can drive a second
/// "boot" without standing up a second app instance.
pub async fn seed_ui_assets(ctx: &AppContext) {
    match planes::store(ctx, store::CONFIG) {
        Ok(mut config) => match config.get(crate::relay::operator::MANIFEST_CONFIG).await {
            Ok(Some(manifest_bytes)) => {
                let etag =
                    serde_json::from_slice::<crate::relay::operator::Manifest>(&manifest_bytes)
                        .map(|m| m.etag)
                        .unwrap_or_else(|_| "unreadable".to_string());
                tracing::info!(
                    "uploaded bundle {etag} is authoritative; embedded seeding skipped — the \
                     bundle EMBEDDED in this deploy is inert, so a UI change ships with \
                     `push-ui.mjs`, not with a redeploy"
                );
                return;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    "web-assets manifest unreadable ({e}); embedded seeding skipped rather than \
                     risk clobbering a pushed bundle"
                );
                return;
            }
        },
        Err(e) => {
            tracing::warn!(
                "no config binding ({e}); embedded seeding skipped rather than risk clobbering \
                 a pushed bundle"
            );
            return;
        }
    }
    let Ok(mut assets) = planes::blob(ctx, store::WEB_ASSETS) else {
        tracing::warn!("no web-assets binding; the web client was not seeded");
        return;
    };
    let mut wrote = 0usize;
    for (key, bytes) in crate::relay::assets::ui::UI_ASSETS {
        match assets.get(key.as_bytes()).await {
            Ok(Some(current)) if current == *bytes => continue,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("web asset {key} read failed: {e}");
                continue;
            }
        }
        match assets.put(key.as_bytes(), bytes).await {
            Ok(_) => wrote += 1,
            Err(e) => tracing::warn!("web asset {key} put failed: {e}"),
        }
    }
    if wrote > 0 {
        tracing::info!("seeded {wrote} web-client object(s) under ui/");
    }
}

/// Create the live feed and changefeed topics if they are not there yet.
///
/// Idempotent at the conductor manifest authority, so every instance may call it
/// on every boot and a race between instances is a no-op for the loser.
pub(super) async fn ensure_feed_topics(ctx: &AppContext) {
    let Ok(mut topics) = planes::topics(ctx, store::FEED) else {
        tracing::warn!("no feed binding; delivery topics were not prepared");
        return;
    };
    for topic in [
        keys::feed_topic(DEFAULT_COMMUNITY, FEED_GENERATION),
        keys::changefeed_topic(DEFAULT_COMMUNITY),
    ] {
        match topics.create_topic(&topic).await {
            Ok((_epoch, created)) => {
                if created {
                    tracing::info!("created delivery topic {topic}");
                }
            }
            Err(e) => tracing::warn!("could not prepare topic {topic}: {e}"),
        }
    }
}

/// The most rooms one boot will reconcile. A deployment with more is not
/// broken — the rest converge on their next membership or metadata change, and
/// the ceiling keeps a cold boot from turning into an unbounded write storm.
pub(super) const MAX_RECONCILED_CHANNELS: usize = 500;

/// The most distinct rooms one boot's membership scan will collect before it
/// stops looking. Scanning is key reads only; this bounds the memory of the
/// room list, not the publish work.
pub(super) const MAX_SCANNED_CHANNELS: usize = 20_000;

/// The starter rooms every new member joins (`ensure_starter_channels` in the
/// synced client).
pub(super) const STARTER_ROOMS: [&str; 2] = ["general", "welcome-everyone"];

/// Order the reconcile so the starter rooms come first, then the rest in scan
/// order, each once.
///
/// The scan walks `mem/{community}/{channel}/…` in key order, so a community
/// with more than [`MAX_RECONCILED_CHANNELS`] rooms — one private Welcome room
/// per member already exceeds it on staging — truncates before `general`
/// (hex UUID ids sort first). Measured on staging 2026-09-12: `general` and
/// `welcome-everyone` had lost their 39000/39001 and a redeploy did not restore
/// them. Every new member's sidebar starts from those two rooms, so a boot
/// must always reconcile them, whatever else it has room for.
pub(super) fn starter_rooms_first(scanned: Vec<String>) -> Vec<String> {
    let mut ordered: Vec<String> = STARTER_ROOMS
        .iter()
        .filter(|room| scanned.iter().any(|seen| seen == *room))
        .map(|room| (*room).to_string())
        .collect();
    ordered.extend(
        scanned
            .into_iter()
            .filter(|room| !STARTER_ROOMS.contains(&room.as_str())),
    );
    ordered
}

/// Republish every room's NIP-29 projections so a client's channel list is
/// complete, including rooms created before an identity was configured.
///
/// The room set comes from the `mem/` rows on the events plane, because that is
/// the only enumerable record of a channel: the channel document lives on the
/// APE plane, which has point reads but no scan. Every room has at least its
/// creator's membership row (`create_channel` writes it in the same call), so
/// the two sets agree.
///
/// Cheap when there is nothing to do: [`crate::relay::effects::publish_projection`]
/// derives the projection and returns early when it is byte-identical to the
/// version the watermark already names, so a boot with no drift costs one read
/// per (room, kind) and writes nothing.
pub(super) async fn reconcile_group_projections(ctx: &AppContext) {
    let community = DEFAULT_COMMUNITY;
    let (Ok(mut config), Ok(mut events), Ok(mut ape), Ok(mut topics)) = (
        planes::store(ctx, store::CONFIG),
        planes::store(ctx, store::EVENTS),
        planes::ape(ctx, store::GUARDS),
        planes::topics(ctx, store::FEED),
    ) else {
        return;
    };
    let Some(seckey) = relay_identity(ctx, &mut config).await else {
        return;
    };

    let prefix = format!("mem/{community}/").into_bytes();
    let mut channels: Vec<String> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut cursor: Option<Vec<u8>> = None;
    loop {
        let page = match store::scan(
            &mut events,
            &prefix,
            cursor.as_deref(),
            store::MAX_SCAN_PAGE,
        )
        .await
        {
            Ok(page) => page,
            Err(e) => {
                tracing::warn!("channel reconcile could not scan memberships: {e}");
                return;
            }
        };
        let (rows, more) = page;
        let Some((last, _, _)) = rows.last() else {
            break;
        };
        cursor = Some(last.clone());
        for (key, _, _) in &rows {
            // `mem/{community}/{channel}/{pubkey}` — take the channel component.
            let Some(tail) = key.strip_prefix(prefix.as_slice()) else {
                continue;
            };
            let Some(cut) = tail.iter().position(|b| *b == b'/') else {
                continue;
            };
            let Some(head) = tail.get(..cut) else {
                continue;
            };
            let Ok(channel) = core::str::from_utf8(head) else {
                continue;
            };
            if seen.insert(channel.to_string()) {
                channels.push(channel.to_string());
            }
        }
        // The scan continues past the ceiling (key rows are cheap to read) so
        // the starter rooms are found wherever they sort; only the PUBLISH is
        // bounded, below.
        if !more || channels.len() >= MAX_SCANNED_CHANNELS {
            break;
        }
    }
    let mut channels = starter_rooms_first(channels);
    if channels.len() > MAX_RECONCILED_CHANNELS {
        tracing::warn!(
            "channel reconcile stopped at {MAX_RECONCILED_CHANNELS} rooms; the rest converge on \
             their next change"
        );
        channels.truncate(MAX_RECONCILED_CHANNELS);
    }

    let now_ms = ctx.now_ms();
    for channel in &channels {
        if let Err(e) = crate::relay::effects::publish_group_projections(
            &mut events,
            &mut ape,
            &mut topics,
            community,
            FEED_GENERATION,
            channel,
            &[
                crate::relay::kind::KIND_NIP29_GROUP_METADATA,
                crate::relay::kind::KIND_NIP29_GROUP_ADMINS,
                crate::relay::kind::KIND_NIP29_GROUP_MEMBERS,
            ],
            &seckey,
            now_ms,
        )
        .await
        {
            tracing::warn!("channel {channel} was not reconciled: {e}");
        }
    }
    tracing::info!(
        "reconciled NIP-29 projections for {} room(s)",
        channels.len()
    );
}

/// Re-publish wakes for schedules that are live and already due.
///
/// Best-effort in every arm, like the other boot reconciles: a schedule that cannot
/// be healed must not stop the app serving, and the next boot tries again. See
/// [`crate::relay::schedules::republish_due`] for why re-creating the schedule is not
/// the repair and this is.
pub(super) async fn reconcile_due_schedules(ctx: &AppContext) {
    let Ok(mut config) = planes::store(ctx, store::CONFIG) else {
        return;
    };
    match crate::relay::schedules::republish_due(ctx, &mut config, DEFAULT_COMMUNITY, ctx.now_ms())
        .await
    {
        Ok((0, _)) => {}
        Ok((healed, complete)) => {
            if complete {
                tracing::info!(
                    "re-published {healed} due schedule wake(s); a row stranded without one \
                     would otherwise have stayed live and inert forever"
                );
            } else {
                // The count is a FLOOR, not a total: the sweep is paged and
                // the cursor is durable, so a subsequent boot resumes from the
                // stored cursor rather than re-walking ground already swept.
                tracing::warn!(
                    "re-published {healed} due schedule wake(s) before the sweep ran out of \
                     pages, so {healed} is a floor rather than a total; the next boot \
                     resumes from where this one stopped"
                );
            }
        }
        Err(e) => tracing::warn!("due-schedule sweep did not run: {e}"),
    }
}

/// The most personas one boot will reconcile — the registry's own ceiling
/// ([`MAX_CLIENT_AGENTS`]), so a full roster is always covered.
pub(super) const MAX_RECONCILED_PERSONAS: usize = MAX_CLIENT_AGENTS;

/// Materialize a registered persona's community membership, create-if-absent.
///
/// `Ok(true)` when this call created the row, `Ok(false)` for the one expected
/// non-creation — the CAS miss that means a row is already there. Anything else is
/// an error rather than a quiet `false`: reporting a failed write as "already a
/// member" would let a caller believe a persona can speak when it cannot.
///
/// See the call in [`reconcile_persona_profiles`] for why a registered persona is
/// entitled to a membership: in a CLOSED community it is what lets any of its
/// writes past `enforce_community_membership`, and the registry is an
/// operator-rooted authority at least as strong as a claimed invite.
///
/// Create-if-absent, never a rewrite: a persona that later claimed an invite of
/// its own, or was given a different role, keeps whatever row it has.
pub(super) async fn admit_persona_to_community(
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    pubkey: &crate::relay::event::PubKey,
    now_ms: u64,
) -> Result<bool> {
    let doc = serde_json::json!({
        "role": "member",
        "claimed_at_ms": now_ms,
        "source": "persona-registry",
    });
    let bytes = store::encode_doc(&doc)?;
    match events
        .put_if_version(&keys::community_mem(community, pubkey), &bytes, None)
        .await
    {
        Ok(Ok(_)) => Ok(true),
        // The row exists already — the ordinary case on every boot after the first.
        Ok(Err(_)) => Ok(false),
        Err(e) => Err(BuzzError::Storage(format!(
            "persona community membership write: {e}"
        ))),
    }
}

/// Publish a kind-0 profile for every registered persona that has none.
///
/// A persona without a profile cannot be MENTIONED, and therefore cannot be asked
/// for anything: a client's mention autocomplete is built from kind 0, so a
/// persona missing one never appears in it, never gets `p`-tagged, and never wakes.
/// Measured on a live deployment: a persona registered through the operator's
/// `buzz-agents` document had no profile while personas created through
/// `POST /buzz/api/agents` did — that route publishes one and the registry path
/// never has.
///
/// At boot, next to [`reconcile_group_projections`], because this is the same class
/// of problem: state that one write path creates and nothing else ever revisits.
/// Cheap when there is nothing to do — one indexed read per persona, and a write
/// only for a persona that has no profile.
///
/// Best-effort in every arm. A persona whose key is unreadable, whose key does not
/// match the registry, or whose profile will not sign is warned about and skipped:
/// serving the workspace matters more than a name, and the next boot tries again.
pub(super) async fn reconcile_persona_profiles(ctx: &AppContext) {
    let community = DEFAULT_COMMUNITY;
    let Ok(mut config) = planes::store(ctx, store::CONFIG) else {
        return;
    };
    let registry = match registry_or_empty(&mut config).await {
        Ok(registry) => registry,
        Err(e) => {
            tracing::warn!(
                "persona profile reconcile could not read the registry: {e}"
            );
            return;
        }
    };
    if registry.personas.is_empty() {
        return;
    }
    let Ok(mut events) = planes::store(ctx, store::EVENTS) else {
        return;
    };

    let now_ms = ctx.now_ms();
    let mut published = 0usize;
    let mut admitted = 0usize;
    for persona in registry.personas.iter().take(MAX_RECONCILED_PERSONAS) {
        // FIRST: the persona must be a community member, or nothing it writes
        // will be accepted at all.
        //
        // `ingest_event`'s first gate is `enforce_community_membership`, and in a
        // CLOSED community (`buzz-community-closed`) an operator-provisioned
        // persona holds no invite-claim row — so not just this profile but every
        // answer it ever tries to post is refused as an outsider's. It is
        // registered, it is dispatchable, a mention wakes it, and then it cannot
        // speak. This reconcile made that reachable; it did not create it.
        //
        // Materialized from the REGISTRY, create-if-absent, exactly as
        // `claim_invite` materializes it from a code. Sound on authority rather
        // than convenient: the registry is written only by the operator plane, or
        // by the create-agent route behind its ownership claim, so "the operator
        // registered this persona" is at least as strong a warrant as a claimed
        // invite — and it is the same authority that decides which pubkey may sign
        // as this persona at all. Note this is NOT a bypass: the persona passes the
        // gate the way a person does, by holding a row, so it stays subject to
        // bans, timeouts, rate limits and per-channel membership.
        // Best-effort here, unlike on the create route: one persona's storage
        // failure must not stop the rest of the roster being admitted, and the
        // next boot retries. Logged with the name, because a persona that is
        // silently not a member is mute in a closed community.
        match admit_persona_to_community(&mut events, community, &persona.pubkey, now_ms).await {
            Ok(true) => admitted = admitted.saturating_add(1),
            Ok(false) => {}
            Err(e) => tracing::warn!(
                "persona {} was not admitted to the community: {e}",
                persona.agent
            ),
        }

        // Already has a profile? The author index answers in one read, and a
        // persona publishes so few kinds that asking for kind 0 specifically is
        // exact.
        match crate::relay::query::by_author(
            &mut events,
            community,
            &persona.pubkey,
            Some(crate::relay::kind::KIND_PROFILE),
            None,
            1,
        )
        .await
        {
            Ok(page) if !page.events.is_empty() => continue,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("persona {} profile check failed: {e}", persona.agent);
                continue;
            }
        }

        // The persona's own signing key, through the SAME resolver the persona
        // command surface uses: two implementations of "which key signs for this
        // persona" would be two chances to disagree about a credential.
        let seckey =
            match crate::relay::persona::persona_seckey(ctx, &mut config, community, persona).await
            {
                Ok(seckey) => seckey,
                Err(e) => {
                    tracing::warn!(
                        "persona {} has no usable key, so it cannot be given a profile: {e}",
                        persona.agent
                    );
                    continue;
                }
            };
        // The key must BE this persona's. A mis-seeded keyring row would otherwise
        // publish a profile under whatever identity that key belongs to.
        if crate::relay::crypto::schnorr::xonly_pubkey(&seckey) != Some(persona.pubkey) {
            tracing::warn!(
                "persona {}'s seeded key does not match its registered pubkey; no profile \
                 was published",
                persona.agent
            );
            continue;
        }

        // `bot: true` is what a client renders the badge from; the agent id serves
        // as both handle and display name, which is what the create-agent route
        // does for a persona created without one.
        let content = serde_json::json!({
            "name": persona.agent,
            "display_name": persona.agent,
            "bot": true,
        })
        .to_string();
        let profile = match Event::sign_new(
            &seckey,
            crate::relay::kind::KIND_PROFILE,
            Vec::new(),
            &content,
            now_ms / 1000,
        ) {
            Ok(profile) => profile,
            Err(e) => {
                tracing::warn!(
                    "persona {} profile could not be signed: {e}",
                    persona.agent
                );
                continue;
            }
        };
        // Through the ordinary ingest chain, so a persona's profile is admitted,
        // indexed and delivered exactly like any other event.
        match ingest_event(ctx, &persona.pubkey, &profile, now_ms).await {
            Ok(_) => published = published.saturating_add(1),
            Err(e) => tracing::warn!(
                "persona {} profile was not accepted: {e}",
                persona.agent
            ),
        }
    }
    if admitted > 0 {
        tracing::info!(
            "admitted {admitted} registered persona(s) to the community; an invite-only \
             workspace can now accept what they write"
        );
    }
    if published > 0 {
        tracing::info!(
            "published {published} missing persona profile(s); they can now be mentioned"
        );
    }
}
