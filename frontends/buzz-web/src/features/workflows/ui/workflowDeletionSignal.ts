/**
 * Workflows deleted in this session. An editor left open on a workflow that
 * was just deleted closes without its unsaved-changes prompt: there is
 * nothing left to keep editing, and "Keep editing" would strand the person on
 * a workflow that no longer exists.
 */
const deletedWorkflowIds = new Set<string>();

export function markWorkflowDeleted(workflowId: string): void {
  deletedWorkflowIds.add(workflowId);
}

export function wasWorkflowDeleted(workflowId: string | null | undefined): boolean {
  return workflowId != null && deletedWorkflowIds.has(workflowId);
}
