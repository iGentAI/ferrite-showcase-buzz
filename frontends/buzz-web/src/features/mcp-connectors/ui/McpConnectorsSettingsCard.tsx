/**
 * The MCP connector settings surface: the authenticate button.
 *
 * Visible to every member, with the controls disabled and the reason stated for
 * anyone who is not an operator. Hiding the section instead would answer "can
 * this workspace reach Linear?" with silence, which reads as the feature not
 * existing; showing it disabled answers the question. Nothing is leaked by that
 * — a row reports whether a connector is connected and never anything about the
 * credential, which does not leave the relay.
 *
 * The authorization round trip is deliberately not disguised. The relay answers
 * its own callback with JSON rather than redirecting back into the app, so there
 * is no event this tab could wait for; after Connect it says what happened and
 * offers the re-read that can actually confirm the result.
 */
import * as React from "react";
import { isTauri } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { toast } from "sonner";
import {
  AlertTriangle,
  CheckCircle2,
  CircleSlash,
  ClipboardCopy,
  ExternalLink,
  Loader2,
  Plug,
  RefreshCw,
  ShieldAlert,
  Wrench,
} from "lucide-react";

import { SettingsSectionHeader } from "@/features/settings/ui/SettingsSectionHeader";
import { cn } from "@/shared/lib/cn";
import { Button } from "@/shared/ui/button";
import {
  authorizeConnector,
  connectorTools,
  disconnectConnector,
  listConnectors,
  validateAuthorizationUrl,
  type ConnectorCatalog,
  type ConnectorRow,
} from "../lib/mcpConnectors";
import { catalogSummary, rowState } from "../lib/connectorRows";

type LoadState =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ready"; catalog: ConnectorCatalog };

/**
 * An authorization this tab started and has not yet confirmed.
 *
 * `opened` records whether a tab was actually obtained. A blocked popup is a
 * normal outcome, not an error, and the panel has to say which happened rather
 * than assert one — telling somebody to finish in a tab that never appeared is
 * worse than telling them to open it.
 */
type PendingAuthorization = {
  id: string;
  name: string;
  url: string;
  opened: boolean;
};

function messageOf(error: unknown, fallback: string): string {
  return error instanceof Error && error.message ? error.message : fallback;
}

export function McpConnectorsSettingsCard() {
  const [load, setLoad] = React.useState<LoadState>({ status: "loading" });
  const [busy, setBusy] = React.useState<string | null>(null);
  const [pending, setPending] = React.useState<PendingAuthorization | null>(null);
  const [refreshing, setRefreshing] = React.useState(false);

  const read = React.useCallback(async () => {
    try {
      const catalog = await listConnectors();
      setLoad({ status: "ready", catalog });
      return catalog;
    } catch (error) {
      setLoad({
        status: "error",
        message: messageOf(error, "The connector catalog could not be read."),
      });
      return null;
    }
  }, []);

  React.useEffect(() => {
    void read();
  }, [read]);

  const refresh = React.useCallback(async () => {
    setRefreshing(true);
    const catalog = await read();
    setRefreshing(false);
    // Clear the pending notice only once the RELAY agrees the credential is
    // there. A popup closing proves nothing about whether the exchange completed.
    if (catalog && pending) {
      const row = catalog.connectors.find((c) => c.id === pending.id);
      if (row?.connected) {
        setPending(null);
        toast.success(`${pending.name} is connected.`);
      }
    }
  }, [pending, read]);

  /**
   * Open the authorization page, from a plain click with nothing awaited.
   *
   * Separate from [`connect`] precisely because it has no async boundary: this is
   * the path that still works when the first attempt was popup-blocked, and
   * `noopener` is safe here since the URL is already known and needs no
   * navigation afterwards.
   */
  const openAuthorizationPage = React.useCallback((url: string) => {
    let safeUrl: string;
    try {
      safeUrl = validateAuthorizationUrl(url);
    } catch (error) {
      toast.error(
        messageOf(error, "The relay returned an invalid authorization URL."),
      );
      return;
    }
    void openUrl(safeUrl).catch((error) => {
      toast.error(
        messageOf(
          error,
          "Could not open the authorization page. Copy the authorization link instead.",
        ),
      );
    });
  }, []);

  const connect = React.useCallback(async (row: ConnectorRow) => {
    setBusy(row.id);
    const desktop = isTauri();
    // Claimed SYNCHRONOUSLY in browser tabs, inside the click, before anything
    // is awaited. In the Tauri desktop shell, `window.open` inside the webview
    // does not open the system browser; `openUrl` from `@tauri-apps/plugin-opener`
    // opens the validated HTTPS authorization URL directly via OS IPC.
    let claimed = desktop ? null : window.open("about:blank", "_blank");
    if (claimed) {
      let severed = false;
      try {
        claimed.opener = null;
        severed = claimed.opener === null;
      } catch {
        severed = false;
      }
      if (!severed) {
        claimed.close();
        claimed = null;
      }
    }
    try {
      const { authorizationUrl } = await authorizeConnector(row.id);
      const safeUrl = validateAuthorizationUrl(authorizationUrl);
      let landed = false;
      if (desktop) {
        await openUrl(safeUrl);
        landed = true;
      } else if (claimed && !claimed.closed) {
        claimed.location.replace(safeUrl);
        landed = true;
      }
      setPending({
        id: row.id,
        name: row.name,
        url: safeUrl,
        opened: landed,
      });
    } catch (error) {
      // There is nothing to consent to, so do not leave a blank tab behind.
      claimed?.close();
      toast.error(
        messageOf(error, `Could not start authorization for ${row.name}.`),
      );
    } finally {
      setBusy(null);
    }
  }, []);

  /** Put the authorization URL on the clipboard, for a browser that will not open it. */
  const copyAuthorizationUrl = React.useCallback(async (url: string) => {
    try {
      await navigator.clipboard.writeText(url);
      toast.success("Authorization link copied.");
    } catch {
      // Clipboard access can be refused outright. Saying so is better than a
      // button that silently does nothing.
      toast.error(
        "This browser would not give access to the clipboard. Use the open button " +
          "instead.",
      );
    }
  }, []);

  const revoke = React.useCallback(
    async (row: ConnectorRow) => {
      setBusy(row.id);
      try {
        const { wasConnected } = await disconnectConnector(row.id);
        toast.success(
          wasConnected
            ? `${row.name} disconnected.`
            : `${row.name} was already disconnected.`,
        );
        setPending((p) => (p?.id === row.id ? null : p));
        await read();
      } catch (error) {
        toast.error(messageOf(error, `Could not disconnect ${row.name}.`));
      } finally {
        setBusy(null);
      }
    },
    [read],
  );

  /**
   * Ask a connected connector what tools it offers.
   *
   * The only thing that distinguishes a WORKING connection from a stored token.
   * `connected` says a credential exists; it cannot say the far side still accepts
   * it — expired, revoked at the provider, or scoped too narrowly all look
   * identical from here.
   *
   * Offered to any member, matching the route: it spends this deployment's egress
   * so it is gated like a write, but it confers no authority over the external
   * account. Somebody debugging why an agent cannot reach Linear should not need
   * operator credentials to find out.
   */
  const checkTools = React.useCallback(async (row: ConnectorRow) => {
    setBusy(row.id);
    try {
      const result = await connectorTools(row.id);
      const tools = (result as { tools?: unknown })?.tools;
      const names = Array.isArray(tools)
        ? tools
            .map((t) => (t as { name?: unknown })?.name)
            .filter((n): n is string => typeof n === "string")
        : [];
      if (names.length === 0) {
        // Answering with no tools is a real answer, and a different one from a
        // refusal: the server is reachable and offers nothing this token may use.
        toast.warning(`${row.name} answered, but advertises no tools for this token.`);
        return;
      }
      const shown = names.slice(0, 4).join(", ");
      toast.success(
        `${row.name}: ${names.length} tool${names.length === 1 ? "" : "s"} — ${shown}` +
          (names.length > 4 ? ", ..." : ""),
      );
    } catch (error) {
      // The relay's own sentence, which already separates the cases that matter:
      // not authenticated, under-authorized, or a destination the platform refused.
      toast.error(messageOf(error, `${row.name} did not answer a tool listing.`));
    } finally {
      setBusy(null);
    }
  }, []);

  if (load.status === "loading") {
    return (
      <section className="flex min-h-0 flex-1 flex-col" data-testid="settings-mcp-connectors">
        <SettingsSectionHeader
          title="Connectors"
          description="External MCP servers this workspace's agents can reach."
        />
        <p className="flex items-center gap-2 py-3 text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" />
          Reading the connector catalog...
        </p>
      </section>
    );
  }

  if (load.status === "error") {
    return (
      <section className="flex min-h-0 flex-1 flex-col" data-testid="settings-mcp-connectors">
        <SettingsSectionHeader
          title="Connectors"
          description="External MCP servers this workspace's agents can reach."
        />
        <p
          className="py-3 text-sm text-destructive"
          data-testid="mcp-connectors-error"
        >
          {load.message}
        </p>
        <div>
          <Button onClick={() => void read()} size="sm" type="button" variant="outline">
            Try again
          </Button>
        </div>
      </section>
    );
  }

  const { catalog } = load;

  return (
    <section
      className="flex min-h-0 flex-1 flex-col overflow-y-auto"
      data-testid="settings-mcp-connectors"
    >
      <SettingsSectionHeader
        title="Connectors"
        description="External MCP servers this workspace's agents can reach. Connecting one grants every agent in the workspace access to that account, so it is an operator action."
      />

      <div className="flex items-center justify-between gap-3 pb-3">
        <p className="text-sm text-muted-foreground" data-testid="mcp-connectors-summary">
          {catalogSummary(catalog.connectors)}
        </p>
        <Button
          disabled={refreshing}
          onClick={() => void refresh()}
          size="sm"
          type="button"
          variant="ghost"
        >
          <RefreshCw className={cn("mr-1.5 h-3.5 w-3.5", refreshing && "animate-spin")} />
          Refresh
        </Button>
      </div>

      {/* A deployment with no catalog egress grant at all. Stated once here rather
          than repeated on all seventeen rows. */}
      {catalog.egressAvailable ? null : (
        <p
          className="mb-3 rounded-lg border border-border/60 bg-muted/40 p-3 text-sm text-muted-foreground"
          data-testid="mcp-no-egress"
        >
          This deployment holds no MCP egress grant, so the relay cannot dial any
          connector. Egress destinations are part of the deploy spec: adding one is
          a spec edit and a redeploy, not a setting.
        </p>
      )}

      {catalog.operator ? null : (
        <p
          className="mb-3 flex items-start gap-2 rounded-lg border border-border/60 bg-muted/40 p-3 text-sm text-muted-foreground"
          data-testid="mcp-not-operator"
        >
          <ShieldAlert className="mt-0.5 h-4 w-4 shrink-0" />
          <span>
            Only an operator can connect or disconnect these. Connecting grants
            every agent in this workspace access to an external account, which is
            authority over another system rather than a personal preference.
          </span>
        </p>
      )}

      {/* The authorization round trip, described rather than animated. */}
      {pending ? (
        <div
          className="mb-3 rounded-lg border border-primary/40 bg-primary/5 p-3 text-sm"
          data-testid="mcp-pending-authorization"
        >
          <p className="font-medium text-foreground">
            {pending.opened
              ? `Finish connecting ${pending.name} in the tab that just opened.`
              : `Open the authorization page to finish connecting ${pending.name}.`}
          </p>
          <p className="mt-1 text-muted-foreground">
            {pending.opened ? null : (
              <>
                Your browser did not allow a new tab, so it has to be opened from
                the button below.{" "}
              </>
            )}
            The relay completes the exchange itself and answers that tab directly,
            so this one is not told when you are done. Come back and refresh to
            confirm.
          </p>
          <div className="mt-2 flex items-center gap-2">
            <Button onClick={() => void refresh()} size="sm" type="button">
              I have finished; check now
            </Button>
            <Button
              data-testid="mcp-open-authorization"
              onClick={() => openAuthorizationPage(pending.url)}
              size="sm"
              type="button"
              variant={pending.opened ? "ghost" : "default"}
            >
              <ExternalLink className="mr-1.5 h-3.5 w-3.5" />
              {pending.opened ? "Reopen" : "Open authorization page"}
            </Button>
            <Button
              data-testid="mcp-copy-authorization"
              onClick={() => void copyAuthorizationUrl(pending.url)}
              size="sm"
              type="button"
              variant="ghost"
            >
              <ClipboardCopy className="mr-1.5 h-3.5 w-3.5" />
              Copy link
            </Button>
            <Button
              onClick={() => setPending(null)}
              size="sm"
              type="button"
              variant="ghost"
            >
              Dismiss
            </Button>
          </div>
        </div>
      ) : null}

      <div className="divide-y divide-border/50 rounded-lg border border-border/50">
        {catalog.connectors.map((row) => {
          const state = rowState(row, { operator: catalog.operator });
          const working = busy === row.id;
          const StatusIcon =
            state.status === "connected"
              ? CheckCircle2
              : state.status === "not granted"
                ? CircleSlash
                : Plug;
          return (
            <div
              className="flex min-w-0 items-start gap-3 p-3"
              data-testid={`mcp-connector-${row.id}`}
              key={row.id}
            >
              <span className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-muted/50">
                <StatusIcon
                  className={cn(
                    "h-4 w-4",
                    state.status === "connected"
                      ? "text-emerald-600"
                      : "text-muted-foreground",
                  )}
                />
              </span>
              <div className="min-w-0 flex-1">
                <p className="flex flex-wrap items-center gap-2 text-sm font-medium text-foreground">
                  {row.name}
                  <span
                    className="text-2xs font-normal text-muted-foreground"
                    data-testid={`mcp-status-${row.id}`}
                  >
                    {state.status}
                  </span>
                  {state.unverifiedEndpoint ? (
                    <span
                      className="inline-flex items-center gap-1 text-2xs font-normal text-amber-600"
                      data-testid={`mcp-unverified-${row.id}`}
                      title="This endpoint appears in a community list rather than in the provider's own documentation."
                    >
                      <AlertTriangle className="h-3 w-3" />
                      endpoint not documented by the provider
                    </span>
                  ) : null}
                </p>
                {row.summary ? (
                  <p className="mt-0.5 text-sm text-muted-foreground">{row.summary}</p>
                ) : null}
                <p className="mt-0.5 truncate text-2xs text-muted-foreground/70">
                  {row.endpoint}
                </p>
                {/* The row's CONDITION. Separate from the reason a control is
                    disabled, because they are not the same sentence. */}
                {state.note ? (
                  <p
                    className="mt-1 text-2xs text-muted-foreground"
                    data-testid={`mcp-note-${row.id}`}
                  >
                    {state.note}
                  </p>
                ) : null}
                {/* Why the control cannot be pressed, when it cannot. */}
                {state.action.reason ? (
                  <p
                    className="mt-1 text-2xs text-muted-foreground"
                    data-testid={`mcp-reason-${row.id}`}
                  >
                    {state.action.reason}
                  </p>
                ) : null}
              </div>
              <div className="flex shrink-0 items-center gap-2">
                {/* Only where a credential exists: on anything else this would be
                    a button whose failure is already known. */}
                {row.connected ? (
                  <Button
                    data-testid={`mcp-check-${row.id}`}
                    disabled={working}
                    onClick={() => void checkTools(row)}
                    size="sm"
                    type="button"
                    variant="ghost"
                  >
                    <Wrench className="mr-1.5 h-3.5 w-3.5" />
                    Check tools
                  </Button>
                ) : null}
                {state.action.kind === "none" ? null : (
                  <Button
                    data-testid={`mcp-${state.action.kind}-${row.id}`}
                    disabled={!state.action.enabled || working}
                    onClick={() =>
                      void (state.action.kind === "connect" ? connect(row) : revoke(row))
                    }
                    size="sm"
                    type="button"
                    variant={state.action.kind === "connect" ? "default" : "outline"}
                  >
                    {working ? (
                      <Loader2 className="mr-1.5 h-3.5 w-3.5 animate-spin" />
                    ) : null}
                    {state.action.kind === "connect" ? "Connect" : "Disconnect"}
                  </Button>
                )}
              </div>
            </div>
          );
        })}
      </div>

      {/* Providers that cannot work from a deployed app. Listed WITH the reason
          rather than omitted: "where is Figma?" deserves an answer, and its
          server runs on the user's own loopback where no deployment can reach it. */}
      {catalog.unavailable.length > 0 ? (
        <div className="mt-4">
          <p className="pb-2 text-xs font-medium text-foreground">Not available here</p>
          <div className="divide-y divide-border/50 rounded-lg border border-border/50">
            {catalog.unavailable.map((row) => (
              <div
                className="p-3"
                data-testid={`mcp-unavailable-${row.id}`}
                key={row.id}
              >
                <p className="text-sm font-medium text-foreground">{row.name}</p>
                <p className="mt-0.5 text-sm text-muted-foreground">{row.reason}</p>
              </div>
            ))}
          </div>
        </div>
      ) : null}
    </section>
  );
}
