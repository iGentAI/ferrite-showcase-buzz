import { useAgentManagement } from "@/features/agents/useAgentManagement";
import { ProjectChannelRequestDialog } from "@/features/projects/ui/ProjectChannelRequestDialog";
import { AgentCardDialogs } from "./AgentCardViewerDialog";
import { AgentDialog } from "./AgentDialog";

/** Global review surfaces opened by owned agents through the Buzz harness. */
export function AgentManagementDialogs() {
  const management = useAgentManagement();
  const requestId = management.request?.requestId;

  return (
    <>
      {management.request?.action === "create" ? (
        <AgentDialog
          definitionError={
            management.error ? new Error(management.error) : null
          }
          description={management.requesterDescription ?? undefined}
          initialValues={management.createInitialValues}
          isDefinitionPending={management.isPending}
          key={requestId}
          mode="definition"
          onOpenChange={(open) => {
            if (!open) management.dismiss(requestId);
          }}
          onSubmitDefinition={management.submitCreate}
          runtimes={management.runtimes}
          runtimeCatalogStatus={management.runtimeCatalogStatus}
          title="Agent request: create agent"
        />
      ) : null}
      {management.request?.action === "update" ? (
        <AgentDialog
          description={management.requesterDescription ?? ""}
          error={management.editError ? new Error(management.editError) : null}
          initialValues={management.editInitialValues}
          isPending={management.isPending}
          key={requestId}
          mode="definition-edit"
          onOpenChange={(open) => {
            if (!open) management.dismiss(requestId);
          }}
          onSubmit={management.submitUpdate}
          open
          runtimes={management.runtimes}
          runtimeCatalogStatus={management.runtimeCatalogStatus}
          submitLabel="Save changes"
          title="Agent request: edit agent"
        />
      ) : null}
      <ProjectChannelRequestDialog />
      <AgentCardDialogs />
    </>
  );
}
