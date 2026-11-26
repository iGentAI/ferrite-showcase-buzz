import { mentionOccurrences } from "./mentionOccurrences";
import type { UserProfileSummary } from "@/shared/api/types";

export const MENTION_REFERENCE_TAG = "mention";

export function getMentionTagPubkey(tag: string[]): string | null {
  if ((tag[0] !== "p" && tag[0] !== MENTION_REFERENCE_TAG) || !tag[1]) {
    return null;
  }

  return tag[1].toLowerCase();
}

/** An edit snapshot supersedes historical notification p-tags for body identity. */
export function mentionIdentityTags(tags: string[][] | undefined): string[][] {
  const source = tags ?? [];
  return source.some((tag) => tag[0] === "buzz:mention-snapshot")
    ? // Annotated references (e.g. immutable agent-address metadata) are not
      // authored body bindings and must not leak into a fresh edit snapshot.
      source.filter(
        (tag) => tag[0] === MENTION_REFERENCE_TAG && tag.length === 2,
      )
    : source;
}

/**
 * All names a profile can be @mentioned by. Message text is matched against
 * the sender's view of the profile at send time (agents and the CLI resolve
 * mentions against `display_name` *or* `name`, and renames happen after the
 * fact), so a single-alias match leaves chips that render but never resolve
 * to a pubkey. Emitting every known alias — display name, kind-0 `name`, and
 * the NIP-05 local part — keeps rendered chips and pubkey resolution in sync.
 *
 * Exported because the clipboard's paste-side trust check has to ask the same
 * question in reverse: a copied `label → pubkey` pair is only believable if
 * the community's own profile for that pubkey answers to that label. Deriving
 * both from this one alias set means a legitimate copy of a chip rendered off
 * an alias cannot be refused for naming that alias.
 */
export function collectProfileAliases(
  profile: UserProfileSummary | undefined,
): string[] {
  if (!profile) {
    return [];
  }

  const aliases: string[] = [];
  const displayName = profile.displayName?.trim();
  if (displayName) {
    aliases.push(displayName);
  }

  const name = profile.name?.trim();
  if (name) {
    aliases.push(name);
  }

  // "_" is the NIP-05 root identifier, not a mentionable handle.
  const nip05Local = profile.nip05Handle?.trim().split("@")[0]?.trim();
  if (nip05Local && nip05Local !== "_") {
    aliases.push(nip05Local);
  }

  return aliases;
}

export type ResolvedMentionProps = {
  /** All literal competitors, including ambiguous aliases without a binding. */
  mentionNames: string[] | undefined;
  mentionPubkeysByName: Record<string, string> | undefined;
};

/**
 * Most qualified `@Name (<pubkey>)` mentions examined per message body. Each
 * one is a literal label that joins the alias set and costs a scan of the
 * whole body, and the body is written by whoever sent the message: without a
 * cap, one 96 KiB event of repeated qualified mentions of a single tagged key
 * froze every reader's render path for seconds (F168). Qualified mentions past
 * the cap render as plain text.
 */
export const MAX_QUALIFIED_MENTIONS = 64;

/**
 * Resolves mention render names and the name→pubkey map for mentioned users
 * from message `p` tags and non-notifying `mention` reference tags, in one
 * pass over the tags.
 *
 * `p` tags drive notification/search semantics. `mention` tags only preserve
 * render metadata for reference-only mentions.
 *
 * Recognition is separate from identity resolution: ambiguous aliases still
 * own their literal ranges but have no map entry. An explicit (possibly empty)
 * map tells renderers to leave those unbound occurrences as plain text.
 */
export function resolveMentionProps(
  tags: string[][] | undefined,
  profiles: Record<string, UserProfileSummary> | undefined,
  content = "",
): ResolvedMentionProps {
  const taggedKeys = new Set(
    mentionIdentityTags(tags)
      .map(getMentionTagPubkey)
      .filter((key): key is string => Boolean(key)),
  );
  const aliases = new Map<string, { displayName: string; keys: Set<string> }>();
  const add = (label: string, key: string) => {
    const normalized = label.toLowerCase();
    const entry = aliases.get(normalized) ?? {
      displayName: label,
      keys: new Set<string>(),
    };
    entry.keys.add(key);
    aliases.set(normalized, entry);
  };
  for (const key of taggedKeys) {
    for (const alias of collectProfileAliases(profiles?.[key])) add(alias, key);
  }

  // Only event-tagged identities can supply qualified bindings. The body is not
  // an authority to add recipients. Keep the historical literal label even if
  // its profile has since been renamed or is not loaded.
  const qualified: Array<{
    displayName: string;
    pubkey: string;
    base: string;
  }> = [];
  // One occurrence scan per distinct label, not per match: labels repeat, and
  // the scan is case-insensitive, so case variants share a result.
  const startsByLabel = new Map<string, Set<number>>();
  let considered = 0;
  for (const match of content.matchAll(
    /@([^@\r\n]+) \(([0-9a-f]{64})\)(?: ((?:[1-9][0-9]+|[2-9])))?/gi,
  )) {
    const pubkey = match[2].toLowerCase();
    if (!taggedKeys.has(pubkey)) continue;
    considered += 1;
    if (considered > MAX_QUALIFIED_MENTIONS) break;
    const displayName = match[0].slice(1);
    const labelKey = displayName.toLowerCase();
    let starts = startsByLabel.get(labelKey);
    if (!starts) {
      starts = new Set(
        mentionOccurrences(content, [{ displayName }]).map(
          (item) => item.start,
        ),
      );
      startsByLabel.set(labelKey, starts);
    }
    if (!starts.has(match.index)) continue;
    qualified.push({ displayName, pubkey, base: match[1].toLowerCase() });
    add(displayName, pubkey);
  }

  // Qualification can narrow an ambiguous base only when that literal is itself
  // an unambiguous winning occurrence, not a prefix of another tagged alias.
  const ownedAliases = new Set(
    mentionOccurrences(content, [...aliases.values()]).flatMap((match) =>
      match.candidates
        .filter((entry) => entry.keys.size === 1)
        .map((entry) => entry.displayName.toLowerCase()),
    ),
  );
  const ownedQualified = qualified.filter((item) =>
    ownedAliases.has(item.displayName.toLowerCase()),
  );
  const names = new Set<string>();
  // No prototype: a profile alias of `__proto__` or `constructor` is just a
  // key, never an inherited member (finding_1bc8ad3e9dcb1efb).
  const pubkeysByName: Record<string, string> = Object.create(null);
  for (const [label, entry] of aliases) {
    let keys = [...entry.keys];
    if (keys.length > 1) {
      // A selected second Scout is explicitly qualified. Only a unique remaining
      // Scout can own the unqualified label; tag iteration order is irrelevant.
      keys = keys.filter(
        (key) =>
          !ownedQualified.some(
            (item) => item.base === label && item.pubkey === key,
          ),
      );
    }
    names.add(entry.displayName);
    if (keys.length === 1) pubkeysByName[label] = keys[0];
  }

  return {
    mentionNames: names.size > 0 ? [...names] : undefined,
    mentionPubkeysByName: names.size > 0 ? pubkeysByName : undefined,
  };
}

export function resolveMentionNames(
  tags: string[][] | undefined,
  profiles: Record<string, UserProfileSummary> | undefined,
): string[] | undefined {
  return resolveMentionProps(tags, profiles).mentionNames;
}

export function resolveMentionPubkeysByName(
  tags: string[][] | undefined,
  profiles: Record<string, UserProfileSummary> | undefined,
): Record<string, string> | undefined {
  return resolveMentionProps(tags, profiles).mentionPubkeysByName;
}
