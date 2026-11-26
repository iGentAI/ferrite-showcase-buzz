import type { TranscriptItem } from "../agentSessionTypes";
import {
  asRecord,
  getToolString,
  parseToolResultValue,
} from "../agentSessionUtils";

export type SentMessageLink = {
  channelId: string;
  messageId: string;
};

export function getSentMessageLink(
  item: Extract<TranscriptItem, { type: "tool" }>,
): SentMessageLink | null {
  if (item.status !== "completed" || item.isError) {
    return null;
  }

  if (item.descriptor?.renderClass !== "message") {
    return null;
  }

  const resultRecord = getMessageSendResultRecord(item.result);
  if (!resultRecord || resultRecord.accepted === false) {
    return null;
  }

  // The message's own destination wins over the channel the agent's turn runs
  // in: an agent in channel A may send to channel B.
  const channelId =
    getToolString(resultRecord, ["channel_id", "channelId"]) ??
    getToolString(item.args, ["channel_id", "channelId"]) ??
    cliChannelFlag(getToolString(item.args, ["command"])) ??
    item.channelId;
  if (!channelId) {
    return null;
  }

  const messageId = getToolString(resultRecord, [
    "event_id",
    "eventId",
    "message_id",
    "messageId",
  ]);
  if (!messageId) {
    return null;
  }

  return {
    channelId,
    messageId,
  };
}

/** The `--channel <id>` (or `--channel=<id>`) of a `buzz messages send` call. */
function cliChannelFlag(command: string | null | undefined): string | null {
  if (!command || !/\bmessages\s+send\b/.test(command)) return null;
  const match = /(?:^|\s)--channel(?:=|\s+)(["']?)([^\s"']+)\1/.exec(command);
  return match?.[2] ?? null;
}

function getMessageSendResultRecord(
  result: string,
): Record<string, unknown> | null {
  const parsed = parseToolResultValue(result);
  const directRecord = asRecord(parsed);
  if (getMessageEventId(directRecord)) {
    return directRecord;
  }

  const stdout = getToolString(directRecord, ["stdout"]);
  if (!stdout) {
    return null;
  }

  const stdoutRecord = asRecord(parseToolResultValue(stdout));
  return getMessageEventId(stdoutRecord) ? stdoutRecord : null;
}

function getMessageEventId(record: Record<string, unknown>) {
  return getToolString(record, [
    "event_id",
    "eventId",
    "message_id",
    "messageId",
  ]);
}
