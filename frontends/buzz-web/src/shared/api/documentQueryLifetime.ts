// HTTP history batches belong to the document, not to any one subscribing
// component. Cancelling a component must not cancel its sibling readers.
const departureReasons = new WeakSet<object>();

export class DocumentQueryLifetime {
  private controller = new AbortController();

  constructor(target: EventTarget | null) {
    target?.addEventListener("pagehide", () => {
      const reason = new DOMException("Document departed", "AbortError");
      departureReasons.add(reason);
      this.controller.abort(reason);
    });
    target?.addEventListener("pageshow", () => {
      if (this.controller.signal.aborted) this.controller = new AbortController();
    });
  }

  get signal(): AbortSignal {
    return this.controller.signal;
  }
}

let documentLifetime: DocumentQueryLifetime | undefined;

export function documentQuerySignal(): AbortSignal {
  documentLifetime ??= new DocumentQueryLifetime(typeof window === "undefined" ? null : window);
  return documentLifetime.signal;
}

export function isDocumentQueryDeparture(error: unknown): boolean {
  return typeof error === "object" && error !== null && departureReasons.has(error);
}
