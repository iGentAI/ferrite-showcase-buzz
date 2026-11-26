export type AgentDefaultsSurfaceState =
  | "loading"
  | "error"
  | "empty"
  | "ready";

export function resolveAgentDefaultsSurfaceState({
  hasError,
  isLoading,
  readyRuntimeCount,
}: {
  hasError: boolean;
  isLoading: boolean;
  readyRuntimeCount: number;
}): AgentDefaultsSurfaceState {
  if (isLoading) return "loading";
  if (hasError) return "error";
  if (readyRuntimeCount === 0) return "empty";
  return "ready";
}
