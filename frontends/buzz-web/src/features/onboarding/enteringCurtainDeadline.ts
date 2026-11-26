export function enteringCurtainRemainingMs(
  updatedAt: string,
  maxWaitMs: number,
  nowMs = Date.now(),
): number {
  const startedAtMs = Date.parse(updatedAt);
  if (!Number.isFinite(startedAtMs)) return maxWaitMs;

  return Math.max(0, maxWaitMs - Math.max(0, nowMs - startedAtMs));
}
