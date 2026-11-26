import type {
  AgentPersona,
  CreatePersonaInput,
  RespondToMode,
  UpdatePersonaInput,
} from "@/shared/api/types";
import { isRelayManagedHarness } from "@/features/agents/lib/relayManagedHarness";
import { runtimeSupportsLlmProviderSelection } from "@/features/agents/ui/agentConfigOptions";
import {
  selectionOnProviderDropdownChange,
  selectionOnRuntimeChange,
} from "@/features/agents/ui/runtimeModelProviderSelection";

export const AGENT_MANAGEMENT_REQUEST = "agent_management_request" as const;

export type AgentManagementCreateRequest = {
  type: typeof AGENT_MANAGEMENT_REQUEST;
  action: "create";
  requestId: string;
  request: {
    channelId: string;
    displayName: string;
    systemPrompt: string;
  };
};

export type AgentManagementUpdateRequest = {
  type: typeof AGENT_MANAGEMENT_REQUEST;
  action: "update";
  requestId: string;
  request: {
    channelId: string;
    agentName: string;
    displayName?: string;
    systemPrompt?: string;
    runtime?: string;
    provider?: string;
    model?: string;
    /**
     * Accepted on the wire for compatibility with harnesses that send it, but
     * never parsed into a request: access policy is changed only by the user
     * (see updateInputFromRequest).
     */
    respondTo?: RespondToMode;
  };
};

export type AgentManagementRequest =
  | AgentManagementCreateRequest
  | AgentManagementUpdateRequest;

function isText(value: unknown): value is string {
  return typeof value === "string" && value.trim().length > 0;
}

function isRespondTo(value: unknown): value is RespondToMode | undefined {
  return value === undefined || value === "owner-only" || value === "anyone";
}

function hasOnlyKeys(
  value: Record<string, unknown>,
  allowed: readonly string[],
) {
  return Object.keys(value).every((key) => allowed.includes(key));
}

/** Parses only the deliberately narrow no-secret agent-management request contract. */
export function parseAgentManagementRequest(
  value: unknown,
): AgentManagementRequest | null {
  if (typeof value !== "object" || value === null) return null;
  const payload = value as Record<string, unknown>;
  if (
    payload.type !== AGENT_MANAGEMENT_REQUEST ||
    !isText(payload.requestId) ||
    (payload.action !== "create" && payload.action !== "update") ||
    typeof payload.request !== "object" ||
    payload.request === null
  ) {
    return null;
  }
  const request = payload.request as Record<string, unknown>;

  if (payload.action === "create") {
    if (!hasOnlyKeys(request, ["channelId", "displayName", "systemPrompt"])) {
      return null;
    }
    if (
      !isText(request.channelId) ||
      !isText(request.displayName) ||
      !isText(request.systemPrompt)
    ) {
      return null;
    }
    return {
      type: AGENT_MANAGEMENT_REQUEST,
      action: "create",
      requestId: payload.requestId,
      request: {
        channelId: request.channelId,
        displayName: request.displayName,
        systemPrompt: request.systemPrompt,
      },
    };
  }

  if (
    !isRespondTo(request.respondTo) ||
    !hasOnlyKeys(request, [
      "channelId",
      "agentName",
      "displayName",
      "systemPrompt",
      "runtime",
      "provider",
      "model",
      "respondTo",
    ]) ||
    !isText(request.channelId) ||
    !isText(request.agentName)
  ) {
    return null;
  }
  const changes = {
    ...(isText(request.displayName)
      ? { displayName: request.displayName }
      : {}),
    ...(isText(request.systemPrompt)
      ? { systemPrompt: request.systemPrompt }
      : {}),
    ...(isText(request.runtime) ? { runtime: request.runtime } : {}),
    ...(isText(request.provider) ? { provider: request.provider } : {}),
    ...(isText(request.model) ? { model: request.model } : {}),
    // `respondTo` is validated above but deliberately dropped: an agent cannot
    // propose who may instruct it. A request carrying nothing else is empty.
  };
  if (Object.keys(changes).length === 0) return null;
  return {
    type: AGENT_MANAGEMENT_REQUEST,
    action: "update",
    requestId: payload.requestId,
    request: {
      channelId: request.channelId,
      agentName: request.agentName,
      ...changes,
    },
  };
}

export function requestTargetsEditablePersona(
  persona: AgentPersona | undefined,
): persona is AgentPersona {
  return Boolean(persona && !persona.sourceTeam);
}

export function createInputFromRequest(
  request: Extract<AgentManagementRequest, { action: "create" }>,
): CreatePersonaInput {
  return {
    displayName: request.request.displayName,
    systemPrompt: request.request.systemPrompt,
  };
}

/**
 * Overlay an approved agent-requested edit without resetting unrequested behavior.
 *
 * A requested `respondTo` is deliberately NOT applied. The result seeds both
 * the dialog's draft and the baseline it diffs against on Save, so an
 * agent-supplied access mode would become the "unchanged" value: touching any
 * other behavior field would then submit it (widening an owner-only agent to
 * "anyone" without the user choosing it), and leaving behavior alone would
 * display it while silently discarding it. Who may instruct an agent changes
 * only through the user's own edit in the access field, so the stored behavior
 * group (including its allowlist) is carried through unchanged.
 */
export function updateInputFromRequest(
  request: Extract<AgentManagementRequest, { action: "update" }>,
  current: UpdatePersonaInput,
): UpdatePersonaInput {
  const changes = request.request;
  // A requested runtime or provider change goes through the same dependent-
  // value rules as the dialog's dropdowns (features/agents/AGENTS.md rule 4):
  // the dialog seeds this as consistent persisted state and does not re-run
  // them, so a model or key the new runtime/provider cannot use would be
  // saved as is.
  let selection = {
    provider: current.provider ?? "",
    model: current.model ?? "",
    isCustomProviderEditing: false,
    isCustomModelEditing: false,
    envVars: current.envVars ?? {},
  };
  const previousRuntime = current.runtime ?? "";
  const nextRuntime = changes.runtime ?? previousRuntime;
  if (changes.runtime !== undefined && nextRuntime !== previousRuntime) {
    selection = selectionOnRuntimeChange(selection, {
      previousRuntime,
      nextRuntime,
      nextRuntimeCanChooseProvider:
        !isRelayManagedHarness(nextRuntime) &&
        nextRuntime.trim().length > 0 &&
        runtimeSupportsLlmProviderSelection(nextRuntime),
      lockedRuntimeReset: "full",
    });
    // A runtime switch clears the previous runtime's model; a model the
    // request names explicitly is the one it wants.
    if (changes.model === undefined) selection.model = "";
  }
  if (changes.provider !== undefined && changes.provider !== selection.provider) {
    selection = selectionOnProviderDropdownChange(selection, {
      runtime: nextRuntime,
      nextValue: changes.provider,
      clearModelWhenApiKeyMissing: false,
    });
    if (changes.model === undefined) selection.model = "";
  }
  return {
    ...current,
    displayName: changes.displayName ?? current.displayName,
    systemPrompt: changes.systemPrompt ?? current.systemPrompt,
    runtime: changes.runtime ?? current.runtime,
    provider: selection.provider,
    model: changes.model ?? selection.model,
    ...(current.envVars === undefined &&
    Object.keys(selection.envVars).length === 0
      ? {}
      : { envVars: selection.envVars }),
  };
}
