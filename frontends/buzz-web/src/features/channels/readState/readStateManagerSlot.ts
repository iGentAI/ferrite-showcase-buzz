import type {
  ContextParentResolver,
  ReadStateManager,
} from "@/features/channels/readState/readStateManager";

type SlotManager = Pick<
  ReadStateManager,
  "ownerPubkey" | "setContextParentResolver"
>;

/**
 * Holds the live ReadStateManager for `useReadState` and the latest
 * thread-to-channel parent resolver.
 *
 * The resolver is kept here, not only on the manager: a child effect can
 * register it before the parent effect has built the manager, and a relay
 * reconnect replaces the manager while the registering child stays mounted.
 * Each attached manager receives the latest resolver at once.
 *
 * `forPubkey` returns the manager only when it belongs to the account being
 * rendered, so between an account switch and the parent effect's teardown no
 * caller reads account A's markers into account B or writes B's into A.
 */
export class ReadStateManagerSlot<M extends SlotManager = ReadStateManager> {
  private manager: M | null = null;
  private resolver: ContextParentResolver | null = null;

  attach(manager: M): void {
    manager.setContextParentResolver(this.resolver);
    this.manager = manager;
  }

  detach(manager: M): void {
    if (this.manager === manager) this.manager = null;
  }

  forPubkey(pubkey: string | undefined): M | null {
    const manager = this.manager;
    if (!manager || !pubkey || manager.ownerPubkey !== pubkey) return null;
    return manager;
  }

  setResolver(resolver: ContextParentResolver | null): void {
    this.resolver = resolver;
    this.manager?.setContextParentResolver(resolver);
  }
}
