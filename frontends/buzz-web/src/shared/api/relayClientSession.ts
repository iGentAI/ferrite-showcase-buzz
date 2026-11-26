import { Channel, invoke } from "@tauri-apps/api/core";
import {
  createAuthEvent,
  getRelayHttpUrl,
  getRelayWsUrl,
  signRelayEvent,
} from "@/shared/api/tauri";
import type { PresenceStatus, RelayEvent } from "@/shared/api/types";
import {
  KIND_STREAM_MESSAGE,
  KIND_TYPING_INDICATOR,
  KIND_USER_STATUS,
  CHANNEL_EVENT_KINDS,
  KIND_CHANNEL_THREAD_SUMMARY,
} from "@/shared/constants/kinds";
import {
  getTextPayload,
  toRelayFrames,
  type ConnectionState,
  type LiveSubscriptionReadiness,
  type PendingEvent,
  type RelaySubscription,
  type RelaySubscriptionFilter,
  type SubscriptionEventBufferItem,
} from "@/shared/api/relayClientShared";
import {
  buildChannelAuxDeletionFilter,
  buildChannelFilter,
  buildChannelHistoryFilter,
  buildGlobalStreamFilter,
  buildVisibleReactionLiveFilter,
} from "@/shared/api/relayChannelFilters";
import {
  clearClosedRetry,
  flushEvents,
  handleRelayClosed,
  handleSubscriptionEose,
  prepareSubscriptionEvent,
} from "@/shared/api/relayClosedRecovery";
import { getChannelReconnectRepairEvents } from "@/shared/api/channelReconnectRepair";
import { replayLiveSubscriptions } from "@/shared/api/relayReconnectReplay";
import { publishSessionEvent } from "@/shared/api/relayEventPublisher";
import {
  activateRateLimitIfSignalled,
  waitForRateLimit,
} from "@/shared/api/relayRateLimitGate";
import {
  fetchChunkedHistory,
  requestFirstEventGated,
  requestHistoryGated,
} from "@/shared/api/relayGateBoundary";
import { RelayConnectionStateEmitter } from "@/shared/api/relayConnectionStateEmitter";
import {
  isServiceRestartClose,
  isWebSocketClose,
  isWebSocketError,
  shouldRefuseConnect,
  shouldScheduleReconnect,
  shouldWaitForScheduledReconnect,
} from "@/shared/api/relayReconnectPolicy";
import { RelayReconnectWaiters } from "@/shared/api/relayReconnectWaiters";
import { RelayStallWatchdog } from "@/shared/api/relayStallWatchdog";
import {
  AUTH_TIMEOUT_MS,
  BACKOFF_RESET_STABLE_MS,
  EVENT_BATCH_MS,
  HISTORY_TIMEOUT_MS,
  PUBLISH_TIMEOUT_MS,
  RECONNECT_BASE_DELAY_MS,
  RECONNECT_MAX_DELAY_MS,
  STALL_CHECK_INTERVAL_MS,
  STALL_IDLE_TIMEOUT_MS,
} from "@/shared/api/relayClientTimings";
import { closeWebSocket } from "@/shared/api/relayWebSocketClose";
import {
  armRelayAuthentication,
  AuthOkTracker,
  type RelayAuthRequest,
} from "@/shared/api/relayAuthPolicy";
import { createRelayInboundBuffer } from "@/shared/api/relayInboundBuffer";
import { buildThreadReferenceTags } from "@/features/messages/lib/threading";
import { BUZZ_AUTHORIZATION_HEADER } from "@/shared/lib/nip98";
import { documentQuerySignal } from "./documentQueryLifetime";
import { matchFilter } from "nostr-tools";
import {
  chunkHttpRelaySubscriptions,
  dispatchHttpRelayEvents,
  groupHttpRelaySubscriptions,
  nextHttpRelayPollDelay,
  type HttpRelayLiveSubscription,
} from "@/shared/api/relayHttpPoller";
import { HttpRelayQueryBatcher } from "@/shared/api/relayHttpQueryBatcher";
import {
  randomUuid,
  sha256Hex as sha256BytesHex,
} from "@/shared/lib/insecureContextCrypto";

// Vite injects import.meta.env in browser builds. The node test loader does
// not, so keep the default WebSocket transport without crashing module load.
const HTTP_RELAY_ONLY = import.meta.env?.VITE_RELAY_TRANSPORT === "http";

async function sha256Hex(value: string): Promise<string> {
  return sha256BytesHex(new TextEncoder().encode(value));
}

/**
 * The relay answered `["OK", id, false, reason]`: it received the event and
 * refused it. Unlike a transport failure this is an answer, so it is not
 * retried over HTTP, and its reason reaches the caller unchanged.
 */
export class RelayEventRejectedError extends Error {
  readonly relayRejected = true;
}

/**
 * Whether a failed WebSocket publish may be retried over HTTP: a transport or
 * timeout failure, or an `auth-required:` refusal from a socket NIP-42 had not
 * yet bound. Any other explicit refusal (`rate-limited:`, `restricted:`,
 * `blocked:`, `invalid:`) is final for this attempt.
 */
export function shouldRetryPublishOverHttp(error: unknown): boolean {
  if (!(error instanceof RelayEventRejectedError)) return true;
  return error.message.startsWith("auth-required:");
}

/**
 * The HTTP fallback's URL for `path`, on the SAME relay the socket uses: the
 * page's own origin is not the relay in a desktop build or a deployment whose
 * relay lives elsewhere, and a fallback must never sign and send a user's
 * events or filters to a different host than the socket would.
 */
async function relayHttpEndpoint(path: string): Promise<string> {
  const base = (await getRelayHttpUrl()).replace(/\/+$/, "");
  return `${base}${path}`;
}

async function publishEventOverHttp(
  event: RelayEvent,
  assertOwnership: () => void,
): Promise<RelayEvent> {
  // The HTTP path shares the relay's quota with the socket: wait out an armed
  // gate rather than spend another request into it.
  await waitForRateLimit();
  assertOwnership();
  const url = await relayHttpEndpoint("/events");
  assertOwnership();
  const payload = JSON.stringify(event);
  const payloadHash = await sha256Hex(payload);
  assertOwnership();
  const auth = await signRelayEvent({
    kind: 27235,
    content: "",
    tags: [
      ["u", url],
      ["method", "POST"],
      ["payload", payloadHash],
      ["nonce", randomUuid()],
    ],
  });
  // Signing awaits the active identity. A community switch during that await
  // cancels this publish before its event can leave over the HTTP fallback.
  assertOwnership();
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      [BUZZ_AUTHORIZATION_HEADER]: `Nostr ${btoa(JSON.stringify(auth))}`,
    },
    body: payload,
    signal: AbortSignal.timeout(PUBLISH_TIMEOUT_MS),
  });
  const body = (await response.json().catch(() => ({}))) as {
    error?: unknown;
    message?: unknown;
  };
  if (!response.ok) {
    const message =
      typeof body.error === "string"
        ? body.error
        : typeof body.message === "string"
          ? body.message
          : `HTTP ${response.status}`;
    activateRateLimitIfSignalled(message);
    throw new Error(message);
  }
  return event;
}

async function queryFiltersOverHttpNow(
  filters: RelaySubscriptionFilter[],
): Promise<RelayEvent[]> {
  const documentSignal = documentQuerySignal();
  documentSignal.throwIfAborted();
  const url = await relayHttpEndpoint("/filter-query");
  documentSignal.throwIfAborted();
  const payload = JSON.stringify(filters);
  const auth = await signRelayEvent({
    kind: 27235,
    content: "",
    tags: [
      ["u", url],
      ["method", "POST"],
      ["payload", await sha256Hex(payload)],
      ["nonce", randomUuid()],
    ],
  });
  documentSignal.throwIfAborted();
  const signal = AbortSignal.any([documentSignal, AbortSignal.timeout(HISTORY_TIMEOUT_MS)]);
  const response = await fetch(url, {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      [BUZZ_AUTHORIZATION_HEADER]: `Nostr ${btoa(JSON.stringify(auth))}`,
    },
    body: payload,
    signal,
  });
  const body = (await response.json().catch(() => {
    // Preserve the exact abort reason instead of turning a cancelled body read
    // into an invalid-JSON error. Genuine timeouts remain reportable failures.
    signal.throwIfAborted();
    return null;
  })) as unknown;
  if (!response.ok || !Array.isArray(body)) {
    throw new Error(
      response.ok
        ? "Relay returned an invalid query response."
        : `HTTP ${response.status}`,
    );
  }
  return body.filter(
    (event): event is RelayEvent =>
      typeof event === "object" &&
      event !== null &&
      "id" in event &&
      typeof (event as { id?: unknown }).id === "string" &&
      filters.some((filter) => matchFilter(filter, event as never)),
  );
}

const httpRelayQueryBatcher = new HttpRelayQueryBatcher<
  RelaySubscriptionFilter,
  RelayEvent
>(
  queryFiltersOverHttpNow,
  (filter, event) => matchFilter(filter, event as never),
  (filter) => JSON.stringify(filter),
  (event) => event.id,
);

async function queryFiltersOverHttp(
  filters: RelaySubscriptionFilter[],
): Promise<RelayEvent[]> {
  // Multi-filter calls are already deliberate batches (the live poller). Only
  // coalesce independent one-filter history reads emitted by sibling hooks.
  if (filters.length !== 1) return queryFiltersOverHttpNow(filters);
  return httpRelayQueryBatcher.query(filters[0]);
}

async function filterEventsOverHttp(
  filter: RelaySubscriptionFilter,
): Promise<RelayEvent[]> {
  return queryFiltersOverHttp([filter]);
}

type UserStatusInput = { text: string; emoji: string; expiresAt?: number };
export class RelayClient {
  private wsId: number | null = null;
  private relayUrl: string | null = null;
  private connectPromise: Promise<number> | null = null;
  private reconnectTimeout: number | null = null;
  private reconnectWaiters = new RelayReconnectWaiters();
  private reconnectDelayMs = RECONNECT_BASE_DELAY_MS;
  private keepAliveRequested = false;
  private authRequest: RelayAuthRequest | null = null;
  private subscriptions = new Map<string, RelaySubscription>();
  private pendingEvents = new Map<string, PendingEvent>();
  private eventBuffer: SubscriptionEventBufferItem[] = [];
  private flushTimeout: number | null = null;
  private reconnectListeners = new Set<() => void>();
  private hasConnectedOnce = false;
  private notifyReconnectListeners = false;
  private onMessageChannel: Channel<unknown> | null = null;
  private connectionGeneration = 0;
  private sessionEpoch = 0;
  private stabilityTimer: number | null = null;
  private visibleChannelId: string | null = null;
  private httpSubscriptions = new Map<
    string,
    HttpRelayLiveSubscription<RelaySubscriptionFilter, RelayEvent>
  >();
  private httpPollTimeout: number | null = null;
  private httpPollDelayMs = 1_000;
  private httpPollInFlight = false;
  private authOkTracker = new AuthOkTracker();
  private terminal = false;

  private connectionStateEmitter = new RelayConnectionStateEmitter("idle");
  private stallWatchdog = new RelayStallWatchdog({
    intervalMs: STALL_CHECK_INTERVAL_MS,
    idleTimeoutMs: STALL_IDLE_TIMEOUT_MS,
    onStall: (error) => {
      this.connectionStateEmitter.set("stalled");
      this.resetConnection(error);
    },
  });
  setVisibleChannelId(id: string | null) {
    this.visibleChannelId = id;
  }
  disconnect() {
    const error = new Error("Relay disconnected for community switch.");

    if (this.reconnectTimeout) {
      window.clearTimeout(this.reconnectTimeout);
      this.reconnectTimeout = null;
    }
    if (this.stabilityTimer !== null) {
      window.clearTimeout(this.stabilityTimer);
      this.stabilityTimer = null;
    }
    this.stallWatchdog.stop();
    this.sessionEpoch++;
    this.connectionGeneration++;
    this.keepAliveRequested = false;
    this.relayUrl = null;
    this.hasConnectedOnce = false;
    this.notifyReconnectListeners = false;
    this.terminal = false;
    this.visibleChannelId = null;
    if (this.httpPollTimeout !== null) {
      window.clearTimeout(this.httpPollTimeout);
      this.httpPollTimeout = null;
    }
    this.httpSubscriptions.clear();
    this.httpPollInFlight = false;
    this.httpPollDelayMs = 1_000;
    this.authOkTracker.reset();
    this.connectionStateEmitter.set("idle");

    if (this.wsId !== null) {
      void closeWebSocket(this.wsId, "community switch");
      this.wsId = null;
    }

    this.connectPromise = null;
    this.reconnectWaiters.settle(error);

    if (this.authRequest) {
      window.clearTimeout(this.authRequest.timeout);
      this.authRequest.reject(error);
      this.authRequest = null;
    }

    for (const [subId, sub] of this.subscriptions) {
      if (sub.mode !== "live") {
        window.clearTimeout(sub.timeout);
        sub.reject(error);
      } else {
        clearClosedRetry(sub);
      }
      this.subscriptions.delete(subId);
    }

    for (const [eventId, pending] of this.pendingEvents) {
      window.clearTimeout(pending.timeout);
      pending.reject(error);
      this.pendingEvents.delete(eventId);
    }

    if (this.flushTimeout !== null) {
      window.clearTimeout(this.flushTimeout);
      this.flushTimeout = null;
    }
    this.eventBuffer = [];
    this.reconnectListeners.clear();
    this.connectionStateEmitter.clear();
    this.onMessageChannel = null;
    this.reconnectDelayMs = RECONNECT_BASE_DELAY_MS;
  }

  async fetchChannelHistory(channelId: string, limit = 50) {
    return this.fetchHistory(buildChannelHistoryFilter(channelId, limit));
  }

  async fetchChannelHistoryBefore(
    channelId: string,
    before: number,
    limit = 50,
  ) {
    return this.fetchHistory(
      buildChannelHistoryFilter(channelId, limit, before),
    );
  }

  async fetchAuxEventsByReference(
    channelId: string,
    referencedEventIds: string[],
    buildFilter: (
      channelId: string,
      eventIds: string[],
    ) => RelaySubscriptionFilter,
  ) {
    return fetchChunkedHistory(
      referencedEventIds,
      (eventIds) => buildFilter(channelId, eventIds),
      (filter) => this.fetchHistory(filter),
    );
  }

  async fetchAuxDeletionEventsForAuxEvents(
    channelId: string,
    auxEventIds: string[],
  ): Promise<RelayEvent[]> {
    return fetchChunkedHistory(
      auxEventIds,
      (eventIds) => buildChannelAuxDeletionFilter(channelId, eventIds),
      (filter) => this.fetchHistory(filter),
    );
  }

  async fetchEvents(filter: RelaySubscriptionFilter): Promise<RelayEvent[]> {
    return this.fetchHistory(filter);
  }

  async fetchFirstEvent(
    filter: RelaySubscriptionFilter,
  ): Promise<RelayEvent | null> {
    if (HTTP_RELAY_ONLY) {
      return (await filterEventsOverHttp({ ...filter, limit: 1 }))[0] ?? null;
    }
    // A read cancelled by a disconnect must not fall through to HTTP, which
    // would sign as whoever the next session's identity is
    // (finding_d9093a2660a7beb3).
    const ownership = this.sessionEpoch;
    const isCurrent = () => ownership === this.sessionEpoch;
    try {
      await this.ensureConnected();
      return await requestFirstEventGated(
        this.subscriptions,
        (payload) => this.sendRaw(payload),
        (subId) => this.closeSubscription(subId),
        filter,
        HISTORY_TIMEOUT_MS,
        isCurrent,
      );
    } catch (error) {
      if (!isCurrent()) throw error;
      return (await filterEventsOverHttp({ ...filter, limit: 1 }))[0] ?? null;
    }
  }

  private async fetchHistory(filter: RelaySubscriptionFilter) {
    if (HTTP_RELAY_ONLY) {
      return filterEventsOverHttp(filter);
    }
    const ownership = this.sessionEpoch;
    const isCurrent = () => ownership === this.sessionEpoch;
    try {
      await this.ensureConnected();
      return await this.requestHistory(filter, isCurrent);
    } catch (error) {
      // See fetchFirstEvent (finding_d9093a2660a7beb3).
      if (!isCurrent()) throw error;
      return filterEventsOverHttp(filter);
    }
  }

  private requestHistory(
    filter: RelaySubscriptionFilter,
    isCurrent?: () => boolean,
  ): Promise<RelayEvent[]> {
    return requestHistoryGated(
      this.subscriptions,
      (payload) => this.sendRaw(payload),
      (subId) => this.closeSubscription(subId),
      filter,
      HISTORY_TIMEOUT_MS,
      isCurrent,
    );
  }

  async sendMessage(
    channelId: string,
    content: string,
    mentionPubkeys: string[] = [],
    extraTags: string[][] = [],
  ) {
    const tags: string[][] = [["h", channelId]];
    for (const pubkey of mentionPubkeys) {
      tags.push(["p", pubkey]);
    }
    for (const tag of extraTags) {
      tags.push(tag);
    }

    const event = await signRelayEvent({
      kind: KIND_STREAM_MESSAGE,
      content: content.trim(),
      tags,
    });

    return this.publishEvent(
      event,
      "Timed out while sending the message.",
      "Failed to send the message.",
    );
  }

  async sendPresence(status: PresenceStatus) {
    const event = await signRelayEvent({
      kind: 20001,
      content: status,
      tags: [],
    });

    return this.publishEvent(
      event,
      "Timed out while updating presence.",
      "Failed to update presence.",
    );
  }

  async sendTypingIndicator(
    channelId: string,
    parentEventId?: string | null,
    rootEventId?: string | null,
  ) {
    // Disconnected: not worth triggering a reconnect for ephemeral typing.
    if (this.wsId === null) {
      return;
    }
    const event = await signRelayEvent({
      kind: KIND_TYPING_INDICATOR,
      content: "",
      tags: buildThreadReferenceTags(
        channelId,
        parentEventId ?? null,
        rootEventId ?? null,
      ),
    });

    // Fire-and-forget: no need to wait for relay acknowledgement.
    void this.sendRaw(["EVENT", event]).catch(() => {});
  }

  async subscribeToChannel(
    channelId: string,
    onEvent: (event: RelayEvent) => void,
  ) {
    return this.subscribe(buildChannelFilter(channelId, 50), onEvent);
  }

  /** Subscribe to channel rows and aux starting now, with no history replay. */
  async subscribeToChannelLive(
    channelId: string,
    onEvent: (event: RelayEvent) => void,
  ) {
    // 39005 rides only this window-store subscription — CHANNEL_EVENT_KINDS'
    // other consumers (unread tracking, cache merges) must never see
    // summary overlays.
    return this.subscribe(
      {
        kinds: [...CHANNEL_EVENT_KINDS, KIND_CHANNEL_THREAD_SUMMARY],
        "#h": [channelId],
        limit: 1000,
        since: Math.floor(Date.now() / 1_000),
      },
      onEvent,
    );
  }

  /**
   * Subscribe to reactions for the message rows currently rendered.
   *
   * NIP-25 reactions reference their target with an `e` tag and do not carry
   * the channel's `h` tag. They therefore cannot ride the ordinary channel
   * live subscription. Keeping this subscription render-scoped avoids a
   * workspace-wide kind:7 poll while still delivering reactions immediately
   * in both WebSocket and authenticated HTTP-polling transports.
   */
  async subscribeToMessageReactionsLive(
    messageIds: string[],
    onEvent: (event: RelayEvent) => void,
  ) {
    if (messageIds.length === 0) {
      return async () => {};
    }
    return this.subscribe(
      buildVisibleReactionLiveFilter(
        messageIds,
        Math.floor(Date.now() / 1_000) - 1,
      ),
      onEvent,
    );
  }

  /**
   * Subscribe to huddle lifecycle events (kinds 48100–48103) for a channel,
   * so HuddleIndicator detects active huddles without being drowned out by
   * regular channel messages. Includes the last 10 historical events.
   */
  async subscribeToHuddleEvents(
    channelId: string,
    onEvent: (event: RelayEvent) => void,
  ) {
    return this.subscribe(
      {
        kinds: [48100, 48101, 48102, 48103],
        "#h": [channelId],
        limit: 100,
      },
      onEvent,
    );
  }

  async subscribeToTypingIndicators(
    channelId: string,
    onEvent: (event: RelayEvent) => void,
  ) {
    return this.subscribe(
      {
        kinds: [KIND_TYPING_INDICATOR],
        "#h": [channelId],
        limit: 10,
        since: Math.floor(Date.now() / 1_000) - 10,
      },
      onEvent,
    );
  }
  async publishUserStatus(status: UserStatusInput): Promise<RelayEvent> {
    const tags: string[][] = [["d", "general"]];
    if (status.emoji) tags.push(["emoji", status.emoji]);
    if (status.expiresAt) tags.push(["expiration", String(status.expiresAt)]);
    const event = await signRelayEvent({
      kind: KIND_USER_STATUS,
      content: status.text,
      tags,
    });
    return this.publishEvent(
      event,
      "Timed out publishing user status",
      "Failed to publish user status",
    );
  }

  /** Subscribe to kind:30315 user status events (live only, no backfill). */
  async subscribeToUserStatusUpdates(onEvent: (event: RelayEvent) => void) {
    return this.subscribe(
      { kinds: [KIND_USER_STATUS], "#d": ["general"], limit: 0 },
      onEvent,
    );
  }

  async subscribeToAllStreamMessages(onEvent: (event: RelayEvent) => void) {
    return this.subscribe(buildGlobalStreamFilter(50), onEvent);
  }

  async subscribeLive(
    filter: RelaySubscriptionFilter,
    onEvent: (event: RelayEvent) => void,
    onReady?: (readiness: LiveSubscriptionReadiness) => void,
    readinessTimeoutMs?: number,
  ) {
    return this.subscribe(filter, onEvent, onReady, readinessTimeoutMs);
  }
  async preconnect() {
    // Explicit re-engagement (reconnect card / community switch): clears the
    // terminal latch and AUTH rejection streak, and bypasses backoff once.
    this.terminal = false;
    this.authOkTracker.reset();
    this.keepAliveRequested = true;
    await this.connectBypassingBackoff();
  }

  /**
   * Environment-driven resume (online/focus/visibility): bypasses a pending
   * backoff timer but preserves the terminal latch and AUTH rejection streak
   * — only `preconnect()` clears those, so resume events during repeated
   * AUTH rejection cannot defeat the consecutive-rejection cap.
   */
  async resumeReconnect() {
    if (this.terminal) return;
    await this.connectBypassingBackoff();
  }

  private async connectBypassingBackoff() {
    if (HTTP_RELAY_ONLY) {
      this.connectionStateEmitter.set("connected");
      this.reconnectWaiters.settle();
      return;
    }
    if (this.reconnectTimeout !== null) {
      window.clearTimeout(this.reconnectTimeout);
      this.reconnectTimeout = null;
    }
    try {
      await this.ensureConnected();
      this.reconnectWaiters.settle();
    } catch (error) {
      this.reconnectWaiters.settle(
        this.normalizeRelayError(error, "Relay reconnect failed."),
      );
      throw error;
    }
  }

  subscribeToReconnects(listener: () => void) {
    this.reconnectListeners.add(listener);
    return () => {
      this.reconnectListeners.delete(listener);
    };
  }

  /** Current connection state — synchronous read. */
  getConnectionState(): ConnectionState {
    return this.connectionStateEmitter.get();
  }

  /**
   * Subscribe to connection-state transitions. The listener fires
   * immediately with the current state, so callers need no separate
   * `getConnectionState()` call to seed their UI.
   */
  subscribeToConnectionState(listener: (state: ConnectionState) => void) {
    return this.connectionStateEmitter.subscribe(listener);
  }

  private async ensureConnected() {
    if (shouldRefuseConnect({ terminal: this.terminal })) {
      // Terminal (e.g. relay rejected auth): refuse until disconnect() or
      // preconnect() clears the latch, else the reconnect-timer catch and
      // the publish/subscribe retry wrappers would race the terminal
      // "disconnected" state back to "reconnecting".
      throw new Error("Relay session is terminal; cannot reconnect.");
    }

    if (this.connectPromise) {
      return this.connectPromise;
    }

    if (this.wsId !== null) {
      return this.connectionGeneration;
    }

    if (
      shouldWaitForScheduledReconnect({
        hasPendingReconnect: this.reconnectTimeout !== null,
      })
    ) {
      // The reconnect coordinator owns outage pacing. Query, publish, and
      // subscription callers must wait for its scheduled attempt instead of
      // clearing the timer and creating an immediate reconnect storm.
      return this.reconnectWaiters.wait().then(() => this.connectionGeneration);
    }

    const connectPromise = this.connect();
    this.connectPromise = connectPromise;

    try {
      return await connectPromise;
    } finally {
      if (this.connectPromise === connectPromise) {
        this.connectPromise = null;
      }
    }
  }

  private async connect() {
    if (this.stabilityTimer !== null) {
      window.clearTimeout(this.stabilityTimer);
      this.stabilityTimer = null;
    }

    this.connectionStateEmitter.set(
      this.hasConnectedOnce ? "reconnecting" : "connecting",
    );
    const generation = ++this.connectionGeneration;
    const inbound = createRelayInboundBuffer(
      async (delivery) => {
        for (const message of toRelayFrames(delivery))
          await this.handleWsMessage(message, generation);
      },
      (error) => {
        if (generation === this.connectionGeneration)
          this.recoverFromSocketFailure(error, "Relay connection errored.");
      },
    );
    this.onMessageChannel = new Channel<unknown>((delivery) =>
      inbound.receive(delivery),
    );
    try {
      if (!this.relayUrl) {
        this.relayUrl = await getRelayWsUrl();
      }
      const wsId = await invoke<number>("plugin:websocket|connect", {
        url: this.relayUrl,
        onMessage: this.onMessageChannel,
        config: {},
      });
      if (generation !== this.connectionGeneration) {
        void closeWebSocket(wsId, "stale connection attempt");
        throw new Error("Relay connection attempt was superseded.");
      }
      this.wsId = wsId;

      const authentication = armRelayAuthentication(
        AUTH_TIMEOUT_MS,
        (request) => {
          this.authRequest = request;
        },
        (error) => {
          this.authRequest = null;
          this.resetConnection(error);
        },
      );

      const drain = inbound.drain();
      await Promise.race([drain, authentication, inbound.overflow]);
      await drain;
      await authentication;
      this.stabilityTimer = window.setTimeout(() => {
        this.stabilityTimer = null;
        this.reconnectDelayMs = RECONNECT_BASE_DELAY_MS;
      }, BACKOFF_RESET_STABLE_MS);

      this.connectionStateEmitter.set("connected");
      await this.replayLiveSubscriptions();
      this.stallWatchdog.start();
      this.emitReconnectIfNeeded();
      return generation;
    } catch (error) {
      const connectionError = this.normalizeRelayError(
        error,
        "Failed to connect to relay.",
      );
      if (generation === this.connectionGeneration) {
        this.resetConnection(connectionError);
      }
      throw connectionError;
    }
  }

  private async subscribe(
    filter: RelaySubscriptionFilter,
    onEvent: (event: RelayEvent) => void,
    onReady?: (readiness: LiveSubscriptionReadiness) => void,
    readinessTimeoutMs = 250,
  ) {
    const ownership = this.sessionEpoch;
    try {
      if (HTTP_RELAY_ONLY) {
        throw new Error("The edge relay uses authenticated HTTP polling.");
      }
      await this.ensureConnected();
    } catch (error) {
      // A subscription cancelled by a disconnect must not register an HTTP
      // poller in the next session (finding_d9093a2660a7beb3).
      if (ownership !== this.sessionEpoch) throw error;
      const liveFilter: RelaySubscriptionFilter = {
        ...filter,
        limit: Math.max(filter.limit ?? 0, 100),
        since: Math.max(
          filter.since ?? 0,
          Math.floor(Date.now() / 1_000) - 1,
        ),
      };
      const subscriptionId = `http-live-${randomUuid()}`;
      this.httpSubscriptions.set(subscriptionId, {
        filter: liveFilter,
        onEvent,
        seen: new Set<string>(),
      });
      this.ensureHttpPolling();
      // A polled subscription has no backlog to drain: it starts "now", so it
      // is caught up the moment it is registered.
      onReady?.("eose");
      return async () => {
        this.httpSubscriptions.delete(subscriptionId);
        if (
          this.httpSubscriptions.size === 0 &&
          this.httpPollTimeout !== null
        ) {
          window.clearTimeout(this.httpPollTimeout);
          this.httpPollTimeout = null;
        }
      };
    }

    const subId = `live-${randomUuid()}`;
    let resolveReady = (_readiness: LiveSubscriptionReadiness) => {};
    const ready = new Promise<void>((resolve) => {
      resolveReady = (readiness) => {
        window.clearTimeout(fallbackTimeout);
        onReady?.(readiness);
        resolve();
      };
    });
    const fallbackTimeout = window.setTimeout(
      () => resolveReady("timeout"),
      readinessTimeoutMs,
    );

    this.subscriptions.set(subId, {
      mode: "live",
      filter,
      onEvent,
      resolveReady,
    });

    try {
      await this.sendRawWithReconnectRetry(
        ["REQ", subId, filter],
        "Failed to restore relay subscription.",
      );
    } catch (error) {
      window.clearTimeout(fallbackTimeout);
      this.subscriptions.delete(subId);
      throw error;
    }
    await ready;

    return async () => {
      const active = this.subscriptions.get(subId);
      if (active?.mode !== "live") {
        return;
      }

      this.subscriptions.delete(subId);
      clearClosedRetry(active);
      await this.closeSubscription(subId);
    };
  }

  private ensureHttpPolling() {
    this.httpPollDelayMs = 1_000;
    this.scheduleHttpPoll(50);
  }

  private scheduleHttpPoll(delayMs: number) {
    if (
      this.httpPollTimeout !== null ||
      this.httpSubscriptions.size === 0
    ) {
      return;
    }
    this.httpPollTimeout = window.setTimeout(() => {
      this.httpPollTimeout = null;
      void this.pollHttpSubscriptions();
    }, delayMs);
  }

  private wakeHttpPolling() {
    this.httpPollDelayMs = 1_000;
    if (this.httpPollTimeout !== null) {
      window.clearTimeout(this.httpPollTimeout);
      this.httpPollTimeout = null;
    }
    this.scheduleHttpPoll(250);
  }

  private async pollHttpSubscriptions() {
    if (this.httpPollInFlight || this.httpSubscriptions.size === 0) return;
    this.httpPollInFlight = true;
    let deliveredEvents = 0;
    try {
      const current = Array.from(this.httpSubscriptions.entries());
      const groups = groupHttpRelaySubscriptions(
        current.map(([, subscription]) => subscription),
        (filter) => JSON.stringify(filter),
      );
      for (const chunk of chunkHttpRelaySubscriptions(groups)) {
        const events = await queryFiltersOverHttp(
          chunk.map((group) => group.filter),
        );
        events.sort(
          (left, right) =>
            left.created_at - right.created_at ||
            left.id.localeCompare(right.id),
        );
        const active = chunk
          .flatMap((group) => group.subscriptions)
          .filter((subscription) =>
            current.some(
              ([id, currentSubscription]) =>
                currentSubscription === subscription &&
                this.httpSubscriptions.get(id) === subscription,
            ),
          );
        deliveredEvents += dispatchHttpRelayEvents(
          active,
          events,
          (filter, event) => matchFilter(filter, event as never),
        );
      }
    } catch {
      // A later shared poll retries with a fresh one-use NIP-98 credential.
    } finally {
      this.httpPollInFlight = false;
      this.httpPollDelayMs = nextHttpRelayPollDelay(
        this.httpPollDelayMs,
        deliveredEvents,
      );
      this.scheduleHttpPoll(this.httpPollDelayMs);
    }
  }

  private async sendRaw(payload: unknown[]) {
    if (this.wsId === null) {
      throw new Error("Relay socket is not connected.");
    }

    await invoke("plugin:websocket|send", {
      id: this.wsId,
      message: {
        type: "Text",
        data: JSON.stringify(payload),
      },
    });
  }

  private async sendRawForGeneration(payload: unknown[], generation: number) {
    if (generation !== this.connectionGeneration || this.wsId === null) {
      throw new Error("Relay publish was superseded by a session change.");
    }
    const wsId = this.wsId;
    await invoke("plugin:websocket|send", {
      id: wsId,
      message: { type: "Text", data: JSON.stringify(payload) },
    });
  }

  private normalizeRelayError(error: unknown, fallbackMessage: string) {
    return error instanceof Error ? error : new Error(fallbackMessage);
  }

  private recoverFromSocketFailure(
    error: unknown,
    fallbackMessage: string,
  ): Error {
    const normalizedError = this.normalizeRelayError(error, fallbackMessage);
    this.resetConnection(normalizedError);
    return normalizedError;
  }

  private async sendRawWithReconnectRetry(
    payload: unknown[],
    fallbackMessage: string,
  ) {
    try {
      await this.sendRaw(payload);
    } catch (error) {
      const normalizedError = this.recoverFromSocketFailure(
        error,
        fallbackMessage,
      );
      try {
        await this.ensureConnected();
        await this.sendRaw(payload);
      } catch (retryError) {
        throw this.recoverFromSocketFailure(
          retryError,
          normalizedError.message,
        );
      }
    }
  }

  private async closeSubscription(subId: string) {
    if (this.wsId === null) {
      return;
    }

    await this.sendRaw(["CLOSE", subId]);
  }

  async publishEvent(
    event: RelayEvent,
    timeoutMessage: string,
    sendErrorMessage: string,
  ) {
    const ownership = this.sessionEpoch;
    const assertOwnership = () => {
      if (ownership !== this.sessionEpoch) {
        throw new Error("Relay disconnected for community switch.");
      }
    };
    if (HTTP_RELAY_ONLY) {
      const published = await publishEventOverHttp(event, assertOwnership);
      this.wakeHttpPolling();
      return published;
    }
    // Wait for the connection the way `subscribe` does. `this.wsId` is assigned
    // inside `connect()` BEFORE the NIP-42 exchange resolves, so a publish
    // issued in that window went out on a socket the relay had not yet bound to
    // a key — and a relay that requires auth to write answers
    // `auth-required: …`, which this client classifies as terminal.
    try {
      await this.ensureConnected();
    } catch {
      return publishEventOverHttp(event, assertOwnership);
    }
    assertOwnership();
    try {
      return await publishSessionEvent(
        {
          generation: () => this.connectionGeneration,
          ownership: () => this.sessionEpoch,
          pendingEvents: this.pendingEvents,
          send: (payload, generation) =>
            this.sendRawForGeneration(payload, generation),
          reconnect: () => this.ensureConnected(),
          normalizeError: (error, fallback) =>
            this.normalizeRelayError(error, fallback),
          recoverSocketFailure: (error, fallback) =>
            this.recoverFromSocketFailure(error, fallback),
        },
        event,
        timeoutMessage,
        sendErrorMessage,
      );
    } catch (error) {
      // The socket can open before NIP-42 has bound it to the browser key, or
      // the relay can accept an EVENT and lose its OK during a reconnect. The
      // HTTP ingest path is idempotent by signed event id, so retrying the same
      // event there safely covers both cases without minting a duplicate. An
      // explicit refusal is the relay's answer and is passed on as it is.
      if (!shouldRetryPublishOverHttp(error)) throw error;
      return publishEventOverHttp(event, assertOwnership);
    }
  }

  private async handleWsMessage(message: unknown, generation: number) {
    if (generation !== this.connectionGeneration) return;
    this.stallWatchdog.recordInbound();

    if (isWebSocketClose(message)) {
      if (isServiceRestartClose(message))
        this.reconnectDelayMs = RECONNECT_BASE_DELAY_MS;
      this.resetConnection(new Error("Relay connection closed."));
      return;
    }
    if (isWebSocketError(message)) {
      this.resetConnection(new Error("Relay connection errored."));
      return;
    }

    const payload = getTextPayload(message);
    if (!payload) {
      return;
    }

    let data: unknown;
    try {
      data = JSON.parse(payload);
    } catch {
      return;
    }

    if (!Array.isArray(data) || data.length === 0) {
      return;
    }

    const [type, ...rest] = data;
    if (type === "AUTH" && typeof rest[0] === "string") {
      await this.handleAuthChallenge(rest[0], generation);
      return;
    }
    if (type === "EVENT" && typeof rest[0] === "string" && rest[1]) {
      this.handleEvent(rest[0], rest[1] as RelayEvent, generation);
      return;
    }

    if (
      type === "OK" &&
      typeof rest[0] === "string" &&
      typeof rest[1] === "boolean"
    ) {
      this.handleOk(
        rest[0],
        rest[1],
        typeof rest[2] === "string" ? rest[2] : "",
      );
      return;
    }

    if (type === "EOSE" && typeof rest[0] === "string") {
      this.handleEose(rest[0], generation);
      return;
    }
    if (type === "CLOSED" && typeof rest[0] === "string") {
      handleRelayClosed({
        subscriptions: this.subscriptions,
        subId: rest[0],
        message: typeof rest[1] === "string" ? rest[1] : "",
        sendReq: (subId, filter) =>
          this.sendRawWithReconnectRetry(
            ["REQ", subId, filter],
            "Failed to restore relay subscription after CLOSED.",
          ),
        closeSubscription: (subId) => this.closeSubscription(subId),
      });
      return;
    }

    if (type === "NOTICE" && typeof rest[0] === "string") {
      // Connection-scoped back-pressure — arm the gate until it expires.
      activateRateLimitIfSignalled(rest[0]);
    }
  }

  private async handleAuthChallenge(challenge: string, generation: number) {
    if (!this.relayUrl) {
      this.relayUrl = await getRelayWsUrl();
    }

    const event = await createAuthEvent({
      challenge,
      relayUrl: this.relayUrl,
    });

    if (generation !== this.connectionGeneration || !this.authRequest) {
      return;
    }

    this.authRequest.pendingEventId = event.id;
    await this.sendRaw(["AUTH", event]);
  }

  private handleEvent(subId: string, event: RelayEvent, generation: number) {
    const subscription = this.subscriptions.get(subId);
    if (!subscription) {
      return;
    }

    if (subscription.mode === "first") {
      subscription.onEvent(event);
      return;
    }

    if (!prepareSubscriptionEvent(subscription, event)) return;
    this.eventBuffer.push({ subId, event, generation });
    this.flushTimeout ??= window.setTimeout(
      () => this.flushEventBuffer(),
      EVENT_BATCH_MS,
    );
  }

  private flushEventBuffer() {
    this.flushTimeout = null;
    const buffer = this.eventBuffer;
    this.eventBuffer = [];

    flushEvents(buffer, this.subscriptions, this.connectionGeneration);
  }

  private handleEose(subId: string, generation: number) {
    this.flushEventBuffer(); // Deliver preceding EVENT frames before EOSE.
    handleSubscriptionEose({
      subscriptions: this.subscriptions,
      subId,
      closeSubscription: (id) => this.closeSubscription(id),
      generation,
    });
  }

  private handleOk(eventId: string, success: boolean, message: string) {
    if (this.authRequest && this.authRequest.pendingEventId === eventId) {
      window.clearTimeout(this.authRequest.timeout);
      const authRequest = this.authRequest;
      this.authRequest = null;

      // Decision table lives in relayAuthPolicy.ts.
      const decision = this.authOkTracker.record(success, message);
      if (decision === "authenticated") {
        authRequest.resolve();
      } else {
        const error = new Error(message || "Relay authentication rejected.");
        authRequest.reject(error);
        this.resetConnection(error, { reconnect: decision === "retry" });
      }

      return;
    }

    const pendingEvent = this.pendingEvents.get(eventId);
    if (!pendingEvent) {
      return;
    }

    window.clearTimeout(pendingEvent.timeout);
    this.pendingEvents.delete(eventId);

    if (success) {
      pendingEvent.resolve(pendingEvent.event);
    } else {
      // Back-pressure now arrives here rather than as a NOTICE: the relay
      // rejects an over-quota EVENT on the OK channel so this pending publish
      // can be settled at all. Unarmed, the send retries into the same quota.
      activateRateLimitIfSignalled(message);
      pendingEvent.reject(
        new RelayEventRejectedError(message || "Relay rejected the event."),
      );
    }
  }

  private hasLiveSubscriptions() {
    return [...this.subscriptions.values()].some((s) => s.mode === "live");
  }

  private async replayLiveSubscriptions() {
    const generation = this.connectionGeneration;
    try {
      await replayLiveSubscriptions({
        subscriptions: this.subscriptions,
        sendRaw: (payload) => this.sendRaw(payload),
        requestRepair: getChannelReconnectRepairEvents,
        generation,
        visibleChannelId: this.visibleChannelId,
        isActive: () => this.connectionGeneration === generation,
      });
    } catch (error) {
      const reconnectError =
        error instanceof Error
          ? error
          : new Error("Failed to restore relay subscriptions.");
      this.resetConnection(reconnectError);
      throw reconnectError;
    }
  }

  private scheduleReconnect() {
    if (
      !shouldScheduleReconnect({
        terminal: this.terminal,
        hasPendingReconnect: this.reconnectTimeout !== null,
        hasLiveSocket: this.wsId !== null,
        keepAliveRequested: this.keepAliveRequested,
        hasLiveSubscriptions: this.hasLiveSubscriptions(),
      })
    ) {
      return;
    }

    // ±25% jitter spreads a fleet's AUTH storms across a 50% window instead
    // of hitting the relay at the same instant.
    const jitter = this.reconnectDelayMs * (0.75 + Math.random() * 0.5);
    const delay = Math.min(jitter, RECONNECT_MAX_DELAY_MS);
    this.reconnectDelayMs = Math.min(
      this.reconnectDelayMs * 2,
      RECONNECT_MAX_DELAY_MS,
    );

    this.reconnectTimeout = window.setTimeout(() => {
      this.reconnectTimeout = null;
      void this.ensureConnected()
        .then(() => this.reconnectWaiters.settle())
        .catch((error) => {
          this.reconnectWaiters.settle(
            this.normalizeRelayError(error, "Relay reconnect failed."),
          );
          this.scheduleReconnect();
        });
    }, delay);
  }

  private emitReconnectIfNeeded() {
    const shouldNotifyReconnectListeners =
      this.hasConnectedOnce && this.notifyReconnectListeners;

    this.hasConnectedOnce = true;
    this.notifyReconnectListeners = false;

    if (!shouldNotifyReconnectListeners) {
      return;
    }

    for (const listener of this.reconnectListeners) {
      try {
        listener();
      } catch (error) {
        console.error("Failed to handle relay reconnect", error);
      }
    }
  }

  private resetConnection(
    error: Error,
    options?: {
      reconnect?: boolean;
    },
  ) {
    this.onMessageChannel = null;
    this.stallWatchdog.stop();
    this.connectionGeneration++;
    if (this.stabilityTimer !== null) {
      window.clearTimeout(this.stabilityTimer);
      this.stabilityTimer = null;
    }
    if (this.flushTimeout !== null) window.clearTimeout(this.flushTimeout);
    this.flushTimeout = null;
    this.eventBuffer = [];

    if (options?.reconnect === false) {
      this.terminal = true;
      this.connectionStateEmitter.set("disconnected");
    } else if (
      // A late retry failure racing a terminal latch must not paint
      // "reconnecting" over the terminal "disconnected" state; stall is a
      // stronger signal than a generic drop and is kept until reconnect.
      !this.terminal &&
      this.connectionStateEmitter.get() !== "stalled"
    ) {
      this.connectionStateEmitter.set("reconnecting");
    }

    if (options?.reconnect !== false && this.hasConnectedOnce) {
      this.notifyReconnectListeners = true;
    }

    if (options?.reconnect === false && this.reconnectTimeout) {
      window.clearTimeout(this.reconnectTimeout);
      this.reconnectTimeout = null;
    }
    if (options?.reconnect === false) {
      this.reconnectWaiters.settle(error);
    }

    if (this.wsId !== null) {
      void closeWebSocket(this.wsId, "connection reset");
    }

    this.wsId = null;

    if (this.authRequest) {
      window.clearTimeout(this.authRequest.timeout);
      this.authRequest.reject(error);
      this.authRequest = null;
    }

    for (const [subId, subscription] of this.subscriptions) {
      if (subscription.mode !== "live") {
        window.clearTimeout(subscription.timeout);
        subscription.reject(error);
        this.subscriptions.delete(subId);
        continue;
      }
      subscription.resolveReady?.("closed");
      subscription.resolveReady = undefined;
      clearClosedRetry(subscription);
    }
    for (const [eventId, pendingEvent] of this.pendingEvents) {
      window.clearTimeout(pendingEvent.timeout);
      pendingEvent.reject(error);
      this.pendingEvents.delete(eventId);
    }
    if (options?.reconnect !== false) {
      this.scheduleReconnect();
    }
  }
}
