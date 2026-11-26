import * as React from "react";
import { AlertCircle, Upload } from "lucide-react";

import type {
  TeamSnapshotImportPreview,
  TeamSnapshotImportResult,
} from "@/shared/api/tauriTeams";
import { Button } from "@/shared/ui/button";
import {
  Dialog,
  DialogClose,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "@/shared/ui/dialog";
import { Separator } from "@/shared/ui/separator";
import {
  deriveImportPhase,
  deriveTeamPreviewMemoryDisclosure,
  getProfileSyncFailures,
} from "./teamSnapshotImport.lib";

type TeamSnapshotImportDialogProps = {
  open: boolean;
  /** Preview data loaded by the caller before opening. */
  preview: TeamSnapshotImportPreview;
  /** True while the confirm mutation is in-flight. */
  isConfirming: boolean;
  /** Set when the confirm mutation has returned a result. */
  result: TeamSnapshotImportResult | null;
  /** Error from the confirm mutation, if any. */
  confirmError: string | null;
  /** Called with keepAllowlist when user clicks Import. */
  onConfirm: (keepAllowlist: boolean) => void;
  onOpenChange: (open: boolean) => void;
};

// ── Component ─────────────────────────────────────────────────────────────────

export function TeamSnapshotImportDialog({
  open,
  preview,
  isConfirming,
  result,
  confirmError,
  onConfirm,
  onOpenChange,
}: TeamSnapshotImportDialogProps) {
  const [keepAllowlist, setKeepAllowlist] = React.useState(false);

  // Reset choice whenever the dialog opens or a new snapshot replaces the one
  // shown: consent to keep one snapshot's allowlist never carries over to
  // another (finding_97921d6853f39ec7).
  // biome-ignore lint/correctness/useExhaustiveDependencies: preview identity is the reset trigger
  React.useEffect(() => {
    if (open) {
      setKeepAllowlist(false);
    }
  }, [open, preview]);

  const phase = deriveImportPhase(result, isConfirming);

  return (
    <Dialog
      onOpenChange={(next) => {
        // Not dismissible while the import is being confirmed: its result or
        // error would land in a closed dialog and be lost.
        if (!next && isConfirming) return;
        onOpenChange(next);
      }}
      open={open}
    >
      <DialogContent
        aria-describedby={undefined}
        className="max-w-md"
        data-testid="team-snapshot-import-dialog"
        showCloseButton={false}
      >
        <DialogHeader className="space-y-0">
          <div className="flex items-center justify-between gap-4">
            <DialogTitle>
              {phase === "result" ? "Team imported" : "Import team snapshot"}
            </DialogTitle>
            <div className="flex items-center gap-2">
              {phase === "preview" ? (
                <>
                  <Button
                    data-testid="team-snapshot-import-confirm"
                    disabled={isConfirming}
                    onClick={() =>
                      onConfirm(preview.hasSourceAllowlist && keepAllowlist)
                    }
                    size="sm"
                    type="button"
                    variant="default"
                  >
                    <Upload className="h-4 w-4" />
                    Import
                  </Button>
                  <DialogClose asChild>
                    <Button
                      disabled={isConfirming}
                      size="sm"
                      type="button"
                      variant="ghost"
                    >
                      Cancel
                    </Button>
                  </DialogClose>
                </>
              ) : (
                <DialogClose asChild>
                  <Button
                    disabled={phase === "confirming"}
                    size="sm"
                    type="button"
                    variant="ghost"
                  >
                    Close
                  </Button>
                </DialogClose>
              )}
            </div>
          </div>
        </DialogHeader>

        <Separator />

        {phase === "preview" ? (
          <div className="space-y-3">
            <PreviewBody
              preview={preview}
              keepAllowlist={keepAllowlist}
              onKeepAllowlistChange={setKeepAllowlist}
            />
            {confirmError ? (
              <div
                className="flex items-start gap-2 rounded-md border border-destructive/30 bg-destructive/10 px-3 py-2 text-sm text-destructive"
                data-testid="team-snapshot-import-confirm-error"
              >
                <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
                <p>{confirmError}</p>
              </div>
            ) : null}
          </div>
        ) : phase === "confirming" ? (
          <div className="py-4 text-center text-sm text-muted-foreground">
            Creating team…
          </div>
        ) : result !== null ? (
          <ResultBody result={result} />
        ) : null}
      </DialogContent>
    </Dialog>
  );
}

// ── Preview body ──────────────────────────────────────────────────────────────

function PreviewBody({
  preview,
  keepAllowlist,
  onKeepAllowlistChange,
}: {
  preview: TeamSnapshotImportPreview;
  keepAllowlist: boolean;
  onKeepAllowlistChange: (v: boolean) => void;
}) {
  const memoryDisclosure = deriveTeamPreviewMemoryDisclosure(preview.members);
  return (
    <div className="space-y-4 py-1">
      {/* Team identity */}
      <div className="space-y-1">
        <p className="text-sm font-medium">{preview.name}</p>
        {preview.description ? (
          <p className="text-xs text-muted-foreground">{preview.description}</p>
        ) : null}
        {preview.instructions ? (
          // Executable configuration is shown in full, never clipped: the
          // instructions run verbatim once imported.
          <pre
            className="max-h-48 overflow-auto whitespace-pre-wrap break-words font-sans text-xs text-muted-foreground"
            data-testid="team-snapshot-import-instructions"
          >
            {preview.instructions}
          </pre>
        ) : null}
      </div>

      <p className="text-sm text-muted-foreground">
        A new team will be created with fresh keypairs for all members. The
        imported team is independent of the source — identity never travels.
      </p>

      {/* Memory section: disclosed before Import, like the single-agent
          import dialog, because the entries are written on confirm. */}
      {memoryDisclosure.kind === "none" ? (
        <p className="text-xs text-muted-foreground">
          No memory included — config only.
        </p>
      ) : (
        <div
          className="flex items-start gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-sm text-amber-700 dark:text-amber-400"
          data-testid="team-snapshot-import-memory-warning"
        >
          <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
          {memoryDisclosure.kind === "counted" ? (
            <p>
              This snapshot includes{" "}
              <strong>
                {memoryDisclosure.total} memory entr
                {memoryDisclosure.total === 1 ? "y" : "ies"}
              </strong>{" "}
              across its members. Memory is stored as plaintext in the file and
              will be restored under the new agents' identities.
            </p>
          ) : (
            <p>
              This snapshot may include memory entries for its members. Any
              memory it carries is stored as plaintext in the file and will be
              restored under the new agents' identities.
            </p>
          )}
        </div>
      )}

      {/* Member list */}
      {preview.members.length > 0 ? (
        <div className="space-y-1">
          <p className="text-sm font-medium">
            Members ({preview.members.length})
          </p>
          <div className="max-h-80 space-y-1 overflow-y-auto rounded-md border border-border p-2">
            {preview.members.map((member, idx) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: member names may duplicate
              <div key={idx} className="flex flex-col gap-0.5 px-1 py-0.5">
                <p className="text-sm font-medium">{member.displayName}</p>
                {member.systemPrompt ? (
                  // The whole prompt, in a bounded scroll box: a clipped
                  // preview could hide trailing instructions from review.
                  <pre
                    className="max-h-40 overflow-auto whitespace-pre-wrap break-words font-sans text-xs text-muted-foreground"
                    data-testid="team-snapshot-import-member-prompt"
                  >
                    {member.systemPrompt}
                  </pre>
                ) : null}
                {member.hasSourceAllowlist ? (
                  <p
                    className="text-xs text-amber-700 dark:text-amber-400"
                    data-testid="team-snapshot-import-member-allowlist-count"
                  >
                    Source allowlist:{" "}
                    {formatAllowlistCount(member.sourceAllowlistCount)}
                  </p>
                ) : null}
              </div>
            ))}
          </div>
        </div>
      ) : null}

      {/* Allowlist section */}
      {preview.hasSourceAllowlist ? (
        <div
          className="space-y-2 rounded-md border border-border p-3"
          data-testid="team-snapshot-import-allowlist-section"
        >
          <p className="text-sm font-medium">Respond-to allowlist</p>
          <p className="text-xs text-muted-foreground">
            This snapshot includes source-environment pubkey allowlists for one
            or more members. Those identities are not meaningful on your relay.
          </p>
          <ul
            className="space-y-0.5 text-xs text-muted-foreground"
            data-testid="team-snapshot-import-allowlist-summary"
          >
            {preview.members
              .filter((member) => member.hasSourceAllowlist)
              .map((member, idx) => (
                // biome-ignore lint/suspicious/noArrayIndexKey: member names may duplicate
                <li key={idx}>
                  {member.displayName}:{" "}
                  {formatAllowlistCount(member.sourceAllowlistCount)}
                </li>
              ))}
          </ul>
          <p className="text-xs text-muted-foreground">
            Keep lets every one of these identities instruct the new agents,
            which run on your machine. This preview cannot show the keys
            themselves, so choose Keep only if you trust whoever made this
            snapshot; Clear is safer.
          </p>
          <div className="flex flex-col gap-1.5">
            <label className="flex cursor-pointer items-center gap-2">
              <input
                checked={!keepAllowlist}
                data-testid="team-snapshot-import-allowlist-clear"
                name="allowlist-choice"
                onChange={() => onKeepAllowlistChange(false)}
                type="radio"
              />
              <span className="text-sm">
                <strong>Clear</strong> — start with empty allowlists (safer)
              </span>
            </label>
            <label className="flex cursor-pointer items-center gap-2">
              <input
                checked={keepAllowlist}
                data-testid="team-snapshot-import-allowlist-keep"
                name="allowlist-choice"
                onChange={() => onKeepAllowlistChange(true)}
                type="radio"
              />
              <span className="text-sm">
                <strong>Keep</strong> — copy source allowlists to new members
              </span>
            </label>
          </div>
        </div>
      ) : null}
    </div>
  );
}

function formatAllowlistCount(count: number): string {
  return `${count} ${count === 1 ? "identity" : "identities"}`;
}

// ── Result body ───────────────────────────────────────────────────────────────

function ResultBody({ result }: { result: TeamSnapshotImportResult }) {
  const totalMemoryErrors = result.members.reduce(
    (sum, m) => sum + m.memoryErrors.length,
    0,
  );
  const totalMemoryWritten = result.members.reduce(
    (sum, m) => sum + m.memoryWritten,
    0,
  );
  const totalMemoryTotal = result.members.reduce(
    (sum, m) => sum + m.memoryTotal,
    0,
  );
  const hasPartialMemory =
    totalMemoryTotal > 0 && totalMemoryWritten < totalMemoryTotal;
  const profileSyncFailures = getProfileSyncFailures(result.members);

  return (
    <div className="space-y-3 py-1">
      <p className="text-sm">
        <span className="font-medium">{result.team.name}</span> was created
        {profileSyncFailures.length > 0
          ? `, but ${profileSyncFailures.length} member${profileSyncFailures.length === 1 ? "" : "s"} failed to publish ${profileSyncFailures.length === 1 ? "a profile" : "profiles"}.`
          : ` successfully with ${result.members.length} member${result.members.length === 1 ? "" : "s"}.`}
      </p>

      {profileSyncFailures.length > 0 ? (
        <div
          className="flex items-start gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-sm text-amber-700 dark:text-amber-400"
          data-testid="team-snapshot-import-profile-sync-errors"
        >
          <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
          <div className="flex flex-col gap-1">
            <p>Profile sync failed for:</p>
            <ul className="mt-1 max-h-32 space-y-0.5 overflow-y-auto text-xs">
              {profileSyncFailures.map((m) => (
                <li key={m.pubkey} className="break-all font-mono">
                  {m.displayName}: {m.profileSyncError}
                </li>
              ))}
            </ul>
          </div>
        </div>
      ) : null}

      {totalMemoryTotal > 0 ? (
        hasPartialMemory ? (
          <div
            className="flex items-start gap-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-sm text-amber-700 dark:text-amber-400"
            data-testid="team-snapshot-import-partial-memory"
          >
            <AlertCircle className="mt-0.5 h-4 w-4 shrink-0" />
            <div className="flex flex-col gap-1">
              <p>
                Memory partially restored: {totalMemoryWritten} of{" "}
                {totalMemoryTotal} entr
                {totalMemoryTotal === 1 ? "y" : "ies"} written across all
                members.
              </p>
              {totalMemoryErrors > 0 ? (
                <ul
                  className="mt-1 max-h-32 space-y-0.5 overflow-y-auto text-xs"
                  data-testid="team-snapshot-import-memory-errors"
                >
                  {result.members.flatMap((member) =>
                    member.memoryErrors.map((err, index) => (
                      <li
                        // biome-ignore lint/suspicious/noArrayIndexKey: error strings may duplicate; pubkey+index is the stable composite key
                        key={`${member.pubkey}:${index}`}
                        className="break-all font-mono"
                      >
                        {member.displayName}: {err}
                      </li>
                    )),
                  )}
                </ul>
              ) : null}
            </div>
          </div>
        ) : (
          <p
            className="text-xs text-muted-foreground"
            data-testid="team-snapshot-import-memory-success"
          >
            {totalMemoryTotal} memory entr
            {totalMemoryTotal === 1 ? "y" : "ies"} restored across all members.
          </p>
        )
      ) : null}
    </div>
  );
}
