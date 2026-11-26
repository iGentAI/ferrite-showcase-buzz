import * as React from "react";
import {
  ReadStateManager,
  type ContextParentResolver,
} from "@/features/channels/readState/readStateManager";
import { ReadStateManagerSlot } from "@/features/channels/readState/readStateManagerSlot";
import type { RelayClient } from "@/shared/api/relayClientSession";

const noopGetTimestamp = () => null;
const noopMarkRead = () => {};
const noopDrainAdvances = (): ReadonlySet<string> => new Set<string>();
const noopSetResolver = () => {};

/**
 * React hook that creates and manages a ReadStateManager instance.
 * Returns no-op functions when pubkey or relayClient are not available.
 */
export function useReadState(
  pubkey: string | undefined,
  relayClient: RelayClient | undefined,
) {
  const [readStateVersion, forceUpdate] = React.useReducer(
    (x: number) => x + 1,
    0,
  );
  const [initializedPubkey, setInitializedPubkey] = React.useState<
    string | null
  >(null);
  const slotRef = React.useRef<ReadStateManagerSlot | null>(null);
  if (slotRef.current === null) slotRef.current = new ReadStateManagerSlot();
  const slot = slotRef.current;

  // Create/destroy manager when pubkey becomes available/changes
  React.useEffect(() => {
    setInitializedPubkey(null);
    if (!pubkey || !relayClient) return;

    let isCancelled = false;
    const manager = new ReadStateManager(pubkey, relayClient);
    slot.attach(manager);

    const unsubscribe = manager.subscribe(() => {
      forceUpdate();
    });

    void manager.initialize().finally(() => {
      if (!isCancelled) {
        setInitializedPubkey(pubkey);
      }
    });

    return () => {
      isCancelled = true;
      unsubscribe();
      manager.destroy();
      slot.detach(manager);
    };
  }, [pubkey, relayClient, slot]);

  // Every accessor goes through `slot.forPubkey(pubkey)`: until the effect
  // has replaced the previous account's manager, these read and write
  // nothing rather than the wrong account's state.
  const getEffectiveTimestamp = React.useCallback(
    (contextId: string): number | null => {
      return slot.forPubkey(pubkey)?.getEffectiveTimestamp(contextId) ?? null;
    },
    [pubkey, slot],
  );

  const getOwnTimestamp = React.useCallback(
    (contextId: string): number | null => {
      return slot.forPubkey(pubkey)?.getOwnTimestamp(contextId) ?? null;
    },
    [pubkey, slot],
  );

  const markContextRead = React.useCallback(
    (contextId: string, unixTimestamp: number): void => {
      slot.forPubkey(pubkey)?.markContextRead(contextId, unixTimestamp);
    },
    [pubkey, slot],
  );

  const seedContextRead = React.useCallback(
    (contextId: string, unixTimestamp: number): void => {
      slot.forPubkey(pubkey)?.seedContextRead(contextId, unixTimestamp);
    },
    [pubkey, slot],
  );

  const drainSyncedAdvances = React.useCallback((): ReadonlySet<string> => {
    return slot.forPubkey(pubkey)?.drainSyncedAdvances() ?? new Set<string>();
  }, [pubkey, slot]);

  // Kept in the slot so it survives manager creation order and recreation.
  const setContextParentResolver = React.useCallback(
    (resolver: ContextParentResolver | null): void => {
      slot.setResolver(resolver);
    },
    [slot],
  );

  const isReady = Boolean(
    pubkey && relayClient && initializedPubkey === pubkey,
  );

  if (!pubkey || !relayClient) {
    return {
      getEffectiveTimestamp: noopGetTimestamp,
      isReady: false,
      markContextRead: noopMarkRead,
      seedContextRead: noopMarkRead,
      drainSyncedAdvances: noopDrainAdvances,
      setContextParentResolver: noopSetResolver,
      readStateVersion: 0,
      getOwnTimestamp: noopGetTimestamp,
    };
  }

  return {
    getEffectiveTimestamp,
    isReady,
    markContextRead,
    seedContextRead,
    drainSyncedAdvances,
    setContextParentResolver,
    readStateVersion,
    getOwnTimestamp,
  };
}
