import { isMap, parseDocument, parse as yamlParse, stringify as yamlStringify } from "yaml";

import { DEFAULT_FORM_STATE, formStateToYaml } from "./workflowFormTypes";

/**
 * Header-level view of a workflow definition.
 *
 * The dialog header (name, name editing, enabled toggle) must stay usable while
 * the body of the definition is still incomplete — a freshly added step has no
 * message text yet, so full form validation fails even though `name` is present
 * and editable. These fields therefore come from the YAML document itself
 * rather than from {@link yamlToFormState}.
 */
export type WorkflowDocumentFields = {
  /** Whether top-level keys can be written back into the document. */
  editable: boolean;
  /** Relay active/paused state, or the optional legacy `enabled` value. */
  enabled: boolean | null;
  /** The top-level `name` scalar, or `null` when absent or not a string. */
  name: string | null;
};

const EMPTY_DOCUMENT_FIELDS: WorkflowDocumentFields = {
  editable: true,
  enabled: null,
  name: null,
};

const UNEDITABLE_DOCUMENT_FIELDS: WorkflowDocumentFields = {
  editable: false,
  enabled: null,
  name: null,
};

/**
 * Reads the header-level fields of a workflow definition without requiring the
 * whole definition to be valid.
 */
export function readWorkflowDocumentFields(
  yaml: string,
): WorkflowDocumentFields {
  if (!yaml.trim()) return EMPTY_DOCUMENT_FIELDS;

  const document = parseDocument(yaml);
  if (document.errors.length > 0 || !isMap(document.contents)) {
    return UNEDITABLE_DOCUMENT_FIELDS;
  }

  const name = document.get("name");
  const enabled = document.get("enabled");
  const isRelayDocument = !document.has("trigger");
  return {
    editable: true,
    enabled: isRelayDocument
      ? document.get("status") !== "paused"
      : typeof enabled === "boolean" ? enabled : null,
    name: typeof name === "string" ? name : null,
  };
}

/** What the dialog header renders for a definition, in a single parse. */
export type WorkflowHeaderState = {
  /** Whether the name can be edited and the enabled toggle can be written. */
  canEdit: boolean;
  enabled: boolean;
  name: string;
};

/**
 * Derives the header presentation from the working definition, falling back to
 * the saved workflow only for what the document itself does not supply.
 */
export function readWorkflowHeaderState(
  yaml: string,
  fallback: { enabled: boolean; name: string | undefined },
): WorkflowHeaderState {
  const fields = readWorkflowDocumentFields(yaml);
  return {
    canEdit: fields.editable,
    enabled: fields.editable ? fields.enabled !== false : fallback.enabled,
    name: fields.name?.trim() || (fallback.name?.trim() ?? ""),
  };
}

/** Writes `name` into the definition, preserving everything else verbatim. */
export function yamlWithWorkflowName(
  yaml: string,
  name: string,
): string | null {
  if (!yaml.trim()) {
    return formStateToYaml({ ...DEFAULT_FORM_STATE, name });
  }

  const document = parseDocument(yaml);
  if (document.errors.length > 0 || !isMap(document.contents)) return null;
  document.set("name", name);
  return document.toString();
}

/** Updates relay `status` or legacy `enabled`, preserving other fields. */
export function yamlWithWorkflowEnabled(
  yaml: string,
  enabled: boolean,
): string | null {
  if (!yaml.trim()) {
    return formStateToYaml({ ...DEFAULT_FORM_STATE, enabled });
  }

  const document = parseDocument(yaml);
  if (document.errors.length > 0 || !isMap(document.contents)) return null;
  if (!document.has("trigger")) {
    // The Ferrite relay admits status, while the legacy desktop dialect uses
    // enabled. Remove a stale legacy flag so the form and relay agree.
    document.set("status", enabled ? "active" : "paused");
    document.delete("enabled");
  } else if (enabled) {
    document.delete("enabled");
  } else {
    document.set("enabled", false);
  }
  return document.toString();
}

// A saved workflow's `definition` is the parsed document plus two fields the
// backend owns (shared/browser-backend/workflows.ts `definitionFromYaml`, and
// the relay's own `Definition` in apps/buzz/src/relay/workflows.rs): `yaml`,
// the author's text verbatim, and `created_at`. Neither is part of what the
// author wrote, and both are rewritten on every save.

const BOOKKEEPING_KEYS = ["yaml", "created_at"] as const;

function plainRecord(value: unknown): Record<string, unknown> | null {
  if (value === null || typeof value !== "object" || Array.isArray(value)) return null;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null
    ? (value as Record<string, unknown>)
    : null;
}

/** The definition without the backend's bookkeeping fields. */
export function stripWorkflowBookkeeping(
  definition: Record<string, unknown>,
): Record<string, unknown> {
  const stripped = { ...definition };
  for (const key of BOOKKEEPING_KEYS) delete stripped[key];
  return stripped;
}

function sameJsonValue(left: unknown, right: unknown): boolean {
  if (Object.is(left, right)) return true;
  if (Array.isArray(left) || Array.isArray(right)) {
    return (
      Array.isArray(left) &&
      Array.isArray(right) &&
      left.length === right.length &&
      left.every((item, index) => sameJsonValue(item, right[index]))
    );
  }
  const leftRecord = plainRecord(left);
  const rightRecord = plainRecord(right);
  if (!leftRecord || !rightRecord) return false;
  const leftKeys = Object.keys(leftRecord).filter(
    (key) => leftRecord[key] !== undefined,
  );
  const rightKeys = Object.keys(rightRecord).filter(
    (key) => rightRecord[key] !== undefined,
  );
  return (
    leftKeys.length === rightKeys.length &&
    leftKeys.every(
      (key) =>
        // Own keys only: `in` walks the prototype chain, so a `constructor`
        // or `__proto__` key in the author's YAML matched an inherited value
        // (finding_a7ce5e780812b434).
        Object.hasOwn(rightRecord, key) &&
        sameJsonValue(leftRecord[key], rightRecord[key]),
    )
  );
}

function parsedRecord(text: string): Record<string, unknown> | null {
  try {
    return plainRecord(yamlParse(text));
  } catch {
    return null;
  }
}

/**
 * The YAML an editor should show for a stored definition.
 *
 * The author's own text when the backend kept it and it still describes this
 * exact definition; otherwise a serialization of the definition with the
 * bookkeeping fields removed. The text is only trusted when it parses back to
 * the same definition, because the stored event is the author's to write: text
 * that disagreed with the steps that actually run must not be what an editor
 * shows. Text that itself carries bookkeeping keys (written by an older client
 * that serialized the whole stored object) is not shown either.
 */
export function workflowDefinitionYaml(
  definition: Record<string, unknown>,
): string {
  const stripped = stripWorkflowBookkeeping(definition);
  const text = definition.yaml;
  if (typeof text === "string" && text.trim()) {
    const parsed = parsedRecord(text);
    if (
      parsed &&
      !BOOKKEEPING_KEYS.some((key) => Object.hasOwn(parsed, key)) &&
      sameJsonValue(parsed, stripped)
    ) {
      return text;
    }
  }
  return yamlStringify(stripped);
}

/**
 * Whether a stored definition is switched on, read the way the backend that
 * runs it reads it: the relay dialect (no `trigger` key) is paused only by
 * `status: paused`, and the legacy desktop dialect by `enabled: false`. The
 * same split as {@link readWorkflowDocumentFields}, for a parsed definition.
 */
export function readWorkflowDefinitionEnabled(
  definition: Record<string, unknown>,
): boolean {
  return definition.trigger === undefined
    ? definition.status !== "paused"
    : definition.enabled !== false;
}
