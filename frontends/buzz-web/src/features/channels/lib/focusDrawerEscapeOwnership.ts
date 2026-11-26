/**
 * Who owns an Escape press while the focus thread drawer is mounted.
 *
 * ## Why the drawer listens at window-capture at all
 *
 * `FocusThreadDrawer` cannot use the app's ordinary `useEscapeKey` for its own
 * dismissal, because that hook yields to `event.defaultPrevented` and the
 * drawer's own contents claim Escape before it runs: the thread composer's
 * mention autocomplete calls `preventDefault()` on Escape
 * (`features/messages/lib/useMentions.ts`), and Tiptap/ProseMirror's editor
 * handlers sit on the contenteditable itself. All of those dispatch in the
 * BUBBLE phase, so a window-CAPTURE listener is the only position that sees the
 * key before them. Upstream added exactly that (block/buzz `9fb1a90b`, "close
 * focused threads on Escape") and locked it in with an e2e that presses Escape
 * with the mention autocomplete open and expects the drawer — not the
 * autocomplete — to win.
 *
 * ## Why the claim has to be scoped
 *
 * Window-capture is the FIRST position in the whole dispatch path. Radix's
 * dismissable layers listen at *document* capture and only the topmost layer
 * registers (`@radix-ui/react-dismissable-layer`), and that handler both
 * `preventDefault()`s and dismisses. So an unconditional
 * `preventDefault() + stopImmediatePropagation()` at window-capture disabled
 * Escape for EVERY dialog, popover, menu and lightbox in the app for as long as
 * any thread drawer was mounted — `stopImmediatePropagation` kept Radix's
 * handler from running at all, and `preventDefault` alone would have been
 * enough to make it skip the dismiss even if it had. Escape is the universal
 * dismiss, so that is both an a11y failure and a trap: the dialog stays open,
 * its overlay keeps swallowing pointer events, and the next unrelated click is
 * what appears to break.
 *
 * The scope that keeps the reason and drops the theft is the dispatch path:
 * claim only when the key was pressed at a node INSIDE the drawer. Nested
 * controls that live in the drawer subtree (composer, autocomplete) are still
 * beaten, exactly as upstream intends. Anything portalled out of the drawer —
 * every Radix layer defaults to `document.body` — is no longer touched, so
 * `Escape` dismisses the topmost layer and the drawer stays put, which is the
 * correct nesting.
 *
 * Escape pressed while focus sits outside the drawer subtree (on the scrim, or
 * nowhere at all because a menu closed with `onCloseAutoFocus` prevented) still
 * closes the thread: `MessageThreadPanel` — the drawer's own child — registers
 * `useEscapeKey(onClose, … || isFocusMode)`, a bubble-phase window listener
 * that fires after Radix has had its chance and yields when Radix took it.
 * That is the fallback this scoping leans on, and it is the well-behaved one.
 */

/** The part of `Node` this decision needs. Structural, so tests need no DOM. */
type EscapeScopeRoot = {
  contains: (other: Node | null) => boolean;
};

type EscapeOwnershipInput = {
  /** The drawer element, or null before its ref is attached. */
  drawer: EscapeScopeRoot | null;
  /** `event.target` — the node the key was dispatched at. */
  target: unknown;
};

/**
 * True when `target` is a DOM node, duck-typed.
 *
 * `instanceof Node` is not usable here: a keydown's target is an EventTarget,
 * which is not always a Node (`window.dispatchEvent` targets `window`), and
 * `Node.prototype.contains` throws a TypeError when handed a non-Node. A throw
 * inside a capture listener would lose the dismissal entirely, so non-nodes are
 * classified rather than passed through.
 */
function isDomNode(value: unknown): value is Node {
  return (
    typeof value === "object" &&
    value !== null &&
    typeof (value as { nodeType?: unknown }).nodeType === "number"
  );
}

/**
 * Whether the focus thread drawer should claim this Escape for itself.
 *
 * Inclusive: the drawer element is its own scope root, which is the common case
 * — the drawer takes focus on mount (`tabIndex={-1}` plus an explicit
 * `focus()`), so an untouched drawer's Escape is dispatched at the drawer
 * itself.
 */
export function focusDrawerOwnsEscape({
  drawer,
  target,
}: EscapeOwnershipInput): boolean {
  if (!drawer) return false;
  if (!isDomNode(target)) return false;
  return drawer.contains(target);
}

/**
 * Builds the drawer's window-capture Escape listener.
 *
 * `getDrawer` is read at event time rather than captured, so the listener can
 * be attached in one effect and still see a ref that attaches later.
 *
 * Nothing at all happens for a key the drawer does not own — in particular no
 * `preventDefault()`, because Radix's dismissable layer refuses to dismiss an
 * already-defaultPrevented Escape and would leave the topmost layer stuck open.
 */
export function createFocusDrawerEscapeHandler(
  getDrawer: () => EscapeScopeRoot | null,
  onClose: () => void,
): (event: KeyboardEvent) => void {
  return function handleEscape(event: KeyboardEvent) {
    if (event.key !== "Escape") return;
    if (!focusDrawerOwnsEscape({ drawer: getDrawer(), target: event.target })) {
      return;
    }
    event.preventDefault();
    event.stopImmediatePropagation();
    onClose();
  };
}
