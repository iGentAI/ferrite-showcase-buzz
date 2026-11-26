import type { AgentCardSendConfirmation } from "@/features/agents/lib/agentCardSendConfirmation";
import type { UserSearchResult } from "@/shared/api/types";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/shared/ui/alert-dialog";
import { Button } from "@/shared/ui/button";

export type PendingCardSend = {
  recipients: UserSearchResult[];
  confirmation: AgentCardSendConfirmation;
};

/**
 * Confirmation for sending a card that carries plaintext memory. The send
 * uses the recipients captured when Send was pressed, which are the ones this
 * dialog names.
 */
export function AgentCardMemorySendConfirmation({
  pending,
  onCancel,
  onConfirm,
}: {
  pending: PendingCardSend | null;
  onCancel: () => void;
  onConfirm: (pending: PendingCardSend) => void;
}) {
  return (
    <AlertDialog
      onOpenChange={(nextOpen) => {
        if (!nextOpen) onCancel();
      }}
      open={pending !== null}
    >
      <AlertDialogContent data-testid="agent-card-share-memory-confirmation">
        <AlertDialogHeader>
          <AlertDialogTitle>Send memories?</AlertDialogTitle>
          <AlertDialogDescription>
            This card includes{" "}
            <strong>plaintext {pending?.confirmation.memoryLabel}</strong>.{" "}
            {pending?.confirmation.recipientAudience}—and anyone with the file
            link—can read it. Once sent, it can't be recalled. Only share with
            people you trust.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel asChild>
            <Button type="button" variant="outline">
              Cancel
            </Button>
          </AlertDialogCancel>
          <AlertDialogAction asChild>
            <Button
              data-testid="agent-card-share-memory-confirm"
              onClick={() => {
                if (pending) onConfirm(pending);
              }}
              type="button"
            >
              Send
            </Button>
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}
