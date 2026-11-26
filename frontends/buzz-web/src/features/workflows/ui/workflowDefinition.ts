import type { Workflow } from "@/shared/api/types";
import {
  scheduleFormFromTrigger,
  SCHEDULE_FREQUENCY_LABELS,
} from "./workflowSchedule";
import {
  ACTION_LABELS,
  TRIGGER_LABELS,
  TRIGGER_TYPES,
} from "./workflowFormTypes";
import type {
  ActionType,
  StepFormState,
  TriggerConfig,
  TriggerType,
} from "./workflowFormTypes";
import { workflowStepDescription } from "./workflowStepDescription";
import { parseConditionExpressions } from "./workflowConditionExpression";
import { workflowTriggerDescription } from "./workflowTriggerDescription";

/**
 * A Ferrite relay definition: no `trigger` key, and the relay's own `status`,
 * `on` or typed steps. A bare record with none of them is read as upstream.
 */
function isRelayDefinition(definition: Record<string, unknown>): boolean {
  if (definition.trigger !== undefined) return false;
  return (
    definition.status !== undefined ||
    definition.on !== undefined ||
    getWorkflowSteps(definition).some((step) => step.type !== undefined)
  );
}

/** The literal phrase a relay message trigger matches, if it has one. */
function relayContains(definition: Record<string, unknown>): string | null {
  const on = asRecord(definition.on);
  return typeof on?.contains === "string" && on.contains.length > 0
    ? on.contains
    : null;
}

/** A step's action: upstream `action`, or the relay's `type`. */
function stepAction(step: Record<string, unknown>): string | null {
  return nonEmptyString(step.action) ?? nonEmptyString(step.type);
}

function asRecord(value: unknown): Record<string, unknown> | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return null;
  }
  return value as Record<string, unknown>;
}

function nonEmptyString(value: unknown): string | null {
  return typeof value === "string" && value.trim().length > 0
    ? value.trim()
    : null;
}

function humanizeIdentifier(value: string): string {
  return value.replaceAll("_", " ").replace(/\s+/g, " ").trim();
}

function getWorkflowSteps(
  definition: Record<string, unknown>,
): Record<string, unknown>[] {
  return Array.isArray(definition.steps)
    ? definition.steps.map(asRecord).filter((step) => step !== null)
    : [];
}

export function getWorkflowTriggerType(
  definition: Record<string, unknown>,
): string | null {
  if (isRelayDefinition(definition)) {
    return definition.on != null ? "message_posted" : "manual";
  }
  return nonEmptyString(asRecord(definition.trigger)?.on);
}

export function getWorkflowPrimaryAction(
  definition: Record<string, unknown>,
): string | null {
  const first = getWorkflowSteps(definition)[0];
  return first ? stepAction(first) : null;
}

export function getWorkflowPrimaryActionEmoji(
  definition: Record<string, unknown>,
): string | null {
  return getWorkflowActionTiles(definition)[0]?.emoji ?? null;
}

export function getWorkflowActionTiles(
  definition: Record<string, unknown>,
): Array<{ action: string; emoji: string | null; key: string }> {
  return getWorkflowSteps(definition).map((step, index) => ({
    action: stepAction(step) ?? "",
    emoji: step.action === "add_reaction" ? nonEmptyString(step.emoji) : null,
    key: nonEmptyString(step.id) ?? `step-${index}`,
  }));
}

export function getWorkflowTriggerEmoji(
  definition: Record<string, unknown>,
): string | null {
  const trigger = asRecord(definition.trigger);
  if (trigger?.on !== "reaction_added") return null;

  const legacyEmoji = nonEmptyString(trigger.emoji);
  if (legacyEmoji) return legacyEmoji;

  const filter = nonEmptyString(trigger.filter);
  if (!filter) return null;
  const conditions = parseConditionExpressions(filter, "reaction_added");
  const emojiCondition = conditions?.find(
    ({ field, operator }) => field === "trigger_emoji" && operator === "equals",
  );
  return nonEmptyString(emojiCondition?.value);
}

export function getWorkflowTriggerConfig(
  definition: Record<string, unknown>,
): TriggerConfig | null {
  // A relay phrase is a literal, not a condition expression, so it is not
  // carried as `filter` (which the descriptions parse as an expression).
  if (isRelayDefinition(definition)) {
    return { on: definition.on != null ? "message_posted" : "manual" };
  }
  const trigger = asRecord(definition.trigger);
  const triggerType = nonEmptyString(trigger?.on);
  if (
    !trigger ||
    !triggerType ||
    !TRIGGER_TYPES.includes(triggerType as TriggerType)
  ) {
    return null;
  }

  return {
    on: triggerType as TriggerType,
    filter: nonEmptyString(trigger.filter) ?? undefined,
    emoji: nonEmptyString(trigger.emoji) ?? undefined,
    cron: nonEmptyString(trigger.cron) ?? undefined,
    interval: nonEmptyString(trigger.interval) ?? undefined,
  };
}

function getScheduleCardClause(trigger: Record<string, unknown>): string {
  const schedule = scheduleFormFromTrigger({
    on: "schedule",
    cron: nonEmptyString(trigger.cron) ?? undefined,
    interval: nonEmptyString(trigger.interval) ?? undefined,
  });

  switch (schedule.frequency) {
    case "daily":
      return `Every day at ${schedule.time} UTC`;
    case "weekly":
      return `Every week at ${schedule.time} UTC`;
    case "monthly":
      return `Every month at ${schedule.time} UTC`;
    case "custom_interval":
      return schedule.customInterval
        ? `Every ${schedule.customInterval}`
        : "On a schedule";
    case "custom_cron":
      return "On a custom schedule";
    default:
      return SCHEDULE_FREQUENCY_LABELS[schedule.frequency];
  }
}

function getTriggerCardClause(
  definition: Record<string, unknown>,
  presentedReaction?: string,
): string {
  if (isRelayDefinition(definition)) {
    const contains = relayContains(definition);
    if (contains) return `When a message containing “${contains}” is posted`;
    return definition.on != null
      ? "When a message is posted"
      : "When started by hand";
  }
  const trigger = asRecord(definition.trigger);
  const triggerType = nonEmptyString(trigger?.on);
  if (!trigger || !triggerType) return "When this workflow starts";

  switch (triggerType) {
    case "message_posted":
      return nonEmptyString(trigger.filter)
        ? "When a matching message is posted"
        : "When a message is posted";
    case "reaction_added": {
      const emoji = nonEmptyString(presentedReaction ?? trigger.emoji);
      return emoji
        ? `When someone reacts with ${emoji}`
        : "When someone adds a reaction";
    }
    case "diff_posted":
      return nonEmptyString(trigger.filter)
        ? "When a matching diff is posted"
        : "When a diff is posted";
    case "webhook":
      return "When a webhook arrives";
    case "schedule":
      return getScheduleCardClause(trigger);
    default:
      return `When ${humanizeIdentifier(triggerType)} happens`;
  }
}

function eventDescriptionCardClause(
  description: string,
  event: "message" | "diff",
): string | null {
  const subject = event === "message" ? "Message" : "Diff";
  const eventPhrase = `${subject} posted`;
  if (!description.startsWith(eventPhrase)) return null;

  const detail = description.slice(eventPhrase.length);
  if (!detail) return `When a ${event} is posted`;
  if (detail.startsWith(" is ") || detail.startsWith(" is not ")) {
    return `When a ${event}${detail}`;
  }
  if (
    detail.startsWith(" containing ") ||
    detail.startsWith(" without ") ||
    detail.startsWith(" starting with ") ||
    detail.startsWith(" ending with ") ||
    detail === " with text"
  ) {
    return `When a ${event}${detail} is posted`;
  }
  return `When a ${event} is posted${detail}`;
}

function textDescriptionCardClause(
  description: string,
  event: "message" | "diff",
): string | null {
  const subject = event === "message" ? "Message" : "Diff";
  if (!description.startsWith(`${subject} `)) return null;
  return `When a ${event}${description.slice(subject.length)}`;
}

function postedValueCardClause(description: string): string | null {
  const postedIndex = description.indexOf(" posted");
  if (postedIndex < 1) return null;

  const value = description.slice(0, postedIndex);
  if (!value.startsWith("“") && !value.startsWith("Anything except ")) {
    return null;
  }
  const normalizedValue = value.startsWith("Anything")
    ? `anything${value.slice("Anything".length)}`
    : value;
  return `When ${normalizedValue} is posted${description.slice(postedIndex + " posted".length)}`;
}

function qualifiedPostedEventCardClause(
  description: string,
  event: "message" | "diff",
): string | null {
  const subject = event === "message" ? "Message" : "Diff";
  const postedIndex = description.indexOf(" posted");
  if (postedIndex < 1) return null;

  const suffix = description.slice(postedIndex + " posted".length);
  const otherTextPrefix = `${subject} with text other than `;
  if (description.startsWith(otherTextPrefix)) {
    const value = description.slice(otherTextPrefix.length, postedIndex);
    return `When a ${event} with text other than ${value} is posted${suffix}`;
  }

  if (description.startsWith(`${subject} with text posted`)) {
    return `When a ${event} with text is posted${suffix}`;
  }
  if (description.startsWith(`${subject} without text posted`)) {
    return `When a ${event} without text is posted${suffix}`;
  }
  return null;
}

function exactValueEventCardClause(
  description: string,
  event: "message" | "diff",
): string | null {
  const subject = event === "message" ? "Message" : "Diff";
  const prefix = `${subject} “`;
  const postedMarker = " is posted";
  if (!description.startsWith(prefix)) return null;

  const postedIndex = description.indexOf(postedMarker);
  if (postedIndex < prefix.length) return null;
  const value = description.slice(subject.length + 1, postedIndex);
  const suffix = description.slice(postedIndex + postedMarker.length);
  return event === "message"
    ? `When ${value} is posted${suffix}`
    : `When a diff ${value} is posted${suffix}`;
}

function getPresentedTriggerCardClause(description?: string): string | null {
  if (!description) return null;

  const eventClause =
    exactValueEventCardClause(description, "message") ??
    exactValueEventCardClause(description, "diff") ??
    qualifiedPostedEventCardClause(description, "message") ??
    qualifiedPostedEventCardClause(description, "diff") ??
    postedValueCardClause(description) ??
    eventDescriptionCardClause(description, "message") ??
    eventDescriptionCardClause(description, "diff") ??
    textDescriptionCardClause(description, "message") ??
    textDescriptionCardClause(description, "diff");
  if (eventClause) return eventClause;

  if (description.startsWith("Reaction added")) {
    return `When a reaction is added${description.slice("Reaction added".length)}`;
  }
  if (description.endsWith(" reaction added")) {
    return `When someone reacts with ${description.slice(0, -" reaction added".length)}`;
  }
  if (
    description.startsWith("Any reaction ") &&
    description.endsWith(" added")
  ) {
    return `When ${description.slice(0, -" added".length).toLocaleLowerCase()} is added`;
  }
  return null;
}

function getActionCardClause(
  step: Record<string, unknown>,
  channelLabel?: string,
): string | null {
  const action = stepAction(step);
  if (!action) return null;

  // Own keys only: a stored `"action": "toString"` must not resolve to an
  // inherited function (finding_ce1bd8345fe09b71).
  const actionLabel = Object.hasOwn(ACTION_LABELS, action)
    ? ACTION_LABELS[action as ActionType]
    : undefined;
  const parsedStep: StepFormState | null = actionLabel
    ? {
        action: action as ActionType,
        id: nonEmptyString(step.id) ?? "step_1",
        duration: nonEmptyString(step.duration) ?? undefined,
        emoji: nonEmptyString(step.emoji) ?? undefined,
        from: nonEmptyString(step.from) ?? undefined,
        message: nonEmptyString(step.message) ?? undefined,
        method: nonEmptyString(step.method) ?? undefined,
        name: nonEmptyString(step.name) ?? undefined,
        text: nonEmptyString(step.text) ?? undefined,
        to: nonEmptyString(step.to) ?? undefined,
        topic: nonEmptyString(step.topic) ?? undefined,
        url: nonEmptyString(step.url) ?? undefined,
        agent: nonEmptyString(step.agent) ?? undefined,
        prompt: nonEmptyString(step.prompt) ?? undefined,
        approver: nonEmptyString(step.approver) ?? undefined,
      }
    : null;
  const detail = parsedStep
    ? workflowStepDescription(parsedStep, {
        channelLabel,
        includeName: false,
      })
    : null;
  const configuredDetail =
    detail &&
    detail !== actionLabel &&
    detail !==
      (actionLabel
        ? `${actionLabel[0]}${actionLabel.slice(1).toLocaleLowerCase()}`
        : null)
      ? detail
      : null;

  switch (action) {
    case "delay": {
      return configuredDetail
        ? `wait ${configuredDetail}`
        : "wait for a moment";
    }
    case "send_message":
      if (!configuredDetail) return "send a channel message";
      return nonEmptyString(step.text)
        ? `send ${configuredDetail}`
        : `send a message in ${configuredDetail}`;
    case "call_webhook":
      return configuredDetail ? `call ${configuredDetail}` : "call a webhook";
    case "send_dm":
      return configuredDetail
        ? `send ${configuredDetail}`
        : "send a direct message";
    case "request_approval":
      return configuredDetail
        ? `request approval: ${configuredDetail}`
        : "request approval";
    case "add_reaction": {
      return configuredDetail
        ? `add a ${configuredDetail} reaction`
        : "add a reaction";
    }
    case "set_channel_topic":
      return configuredDetail
        ? `set the channel topic to ${configuredDetail}`
        : "update the channel topic";
    case "post":
      return configuredDetail ? `post ${configuredDetail}` : "post a message";
    case "agent":
      return configuredDetail ? `ask ${configuredDetail}` : "ask an agent";
    case "approval":
      return configuredDetail
        ? `request approval from ${configuredDetail}`
        : "request approval";
    default: {
      return (actionLabel ?? humanizeIdentifier(action)).toLocaleLowerCase();
    }
  }
}

export function getWorkflowStepCount(
  definition: Record<string, unknown>,
): number {
  return getWorkflowSteps(definition).length;
}

/** Build a short plain-language label from the workflow's trigger and steps. */
export function getWorkflowCardLabel(
  definition: Record<string, unknown>,
  options: {
    actionChannelLabel?: string;
    triggerDescription?: string;
    triggerReaction?: string;
  } = {},
): string {
  const triggerConfig = getWorkflowTriggerConfig(definition);
  const triggerClause =
    getPresentedTriggerCardClause(options.triggerDescription) ??
    getPresentedTriggerCardClause(
      triggerConfig?.filter && !options.triggerReaction
        ? workflowTriggerDescription(triggerConfig, {
            omitUnresolvedReferences: true,
          })
        : undefined,
    ) ??
    getTriggerCardClause(definition, options.triggerReaction);
  const steps = getWorkflowSteps(definition);
  const firstAction = steps[0]
    ? getActionCardClause(steps[0], options.actionChannelLabel)
    : null;
  if (!firstAction) return triggerClause;

  const remainingStepCount = steps.length - 1;
  if (remainingStepCount === 0) return `${triggerClause}, ${firstAction}`;

  return `${triggerClause}, ${firstAction}, then ${remainingStepCount} more ${
    remainingStepCount === 1 ? "step" : "steps"
  }`;
}

export function getWorkflowDescription(
  definition: Record<string, unknown>,
): string | null {
  return nonEmptyString(definition.description);
}

export function getWorkflowEnabled(
  definition: Record<string, unknown>,
): boolean {
  // A relay definition's `status` is authoritative; a leftover `enabled`
  // flag on it is ignored, as the relay ignores it.
  return isRelayDefinition(definition)
    ? definition.status !== "paused"
    : definition.enabled !== false;
}

export function withWorkflowEnabled(
  definition: Record<string, unknown>,
  enabled: boolean,
): Record<string, unknown> {
  const updated = { ...definition };
  if (isRelayDefinition(definition)) {
    delete updated.enabled;
    updated.status = enabled ? "active" : "paused";
    return updated;
  }
  if (enabled) {
    delete updated.enabled;
  } else {
    updated.enabled = false;
  }
  return updated;
}

export function getWorkflowDisplayStatus(
  workflow: Workflow,
): Workflow["status"] | "disabled" {
  if (workflow.status !== "active") {
    return workflow.status;
  }

  return getWorkflowEnabled(workflow.definition) ? workflow.status : "disabled";
}

export function getWorkflowTriggerSummary(
  definition: Record<string, unknown>,
): string | null {
  if (isRelayDefinition(definition)) {
    const contains = relayContains(definition);
    if (contains) return `Message containing “${contains}”`;
    return definition.on != null
      ? TRIGGER_LABELS.message_posted
      : TRIGGER_LABELS.manual;
  }
  const trigger = getWorkflowTriggerConfig(definition);
  if (trigger)
    return workflowTriggerDescription(trigger, {
      omitUnresolvedReferences: true,
    });

  const rawTrigger = asRecord(definition.trigger);
  const triggerType = nonEmptyString(rawTrigger?.on);
  return triggerType
    ? Object.hasOwn(TRIGGER_LABELS, triggerType)
      ? TRIGGER_LABELS[triggerType as TriggerType]
      : triggerType
    : null;
}
