import * as React from "react";
import { Check, ChevronDown, Search } from "lucide-react";

import { cn } from "@/shared/lib/cn";
import { Popover, PopoverContent, PopoverTrigger } from "@/shared/ui/popover";
import {
  type PersonaDropdownOption,
  PERSONA_FIELD_CONTROL_CLASS,
  PERSONA_FIELD_SHELL_CLASS,
} from "./agentConfigOptions";
import {
  firstEnabledIndex,
  nextEnabledIndex,
  optionsSignature,
} from "./personaModelComboboxState";

type PersonaModelComboboxProps = {
  disabled?: boolean;
  id: string;
  onValueChange: (value: string) => void;
  options: readonly PersonaDropdownOption[];
  placeholder: string;
  value: string;
};

export function PersonaModelCombobox({
  disabled,
  id,
  onValueChange,
  options,
  placeholder,
  value,
}: PersonaModelComboboxProps) {
  const [open, setOpen] = React.useState(false);
  const [query, setQuery] = React.useState("");
  const [highlightedIndex, setHighlightedIndex] = React.useState(0);

  const selectedOption = options.find((option) => option.value === value);

  const filteredOptions = React.useMemo(() => {
    if (query.trim() === "") return options;
    const lower = query.toLowerCase();
    return options.filter((option) =>
      option.label.toLowerCase().includes(lower),
    );
  }, [options, query]);

  // Keyed on what the list CONTAINS, never on the array's identity: the options
  // prop is rebuilt inline by `AgentDefinitionDialog`, so a parent re-render
  // hands us an equal-but-new array and must not move the user's highlight.
  // `personaModelComboboxState` carries the full argument and its tests.
  const signature = optionsSignature(filteredOptions);
  const firstEnabled = firstEnabledIndex(filteredOptions);

  function handleOpenChange(next: boolean) {
    setOpen(next);
    if (!next) {
      setQuery("");
      setHighlightedIndex(0);
    }
  }

  function selectOption(optionValue: string) {
    onValueChange(optionValue);
    handleOpenChange(false);
  }

  function handleKeyDown(event: React.KeyboardEvent) {
    switch (event.key) {
      case "ArrowDown": {
        event.preventDefault();
        if (filteredOptions.length > 0) {
          setHighlightedIndex((i) => nextEnabledIndex(filteredOptions, i, 1));
        }
        break;
      }
      case "ArrowUp": {
        event.preventDefault();
        if (filteredOptions.length > 0) {
          setHighlightedIndex((i) => nextEnabledIndex(filteredOptions, i, -1));
        }
        break;
      }
      case "Enter": {
        event.preventDefault();
        const target = filteredOptions[highlightedIndex];
        if (target && !target.disabled) selectOption(target.value);
        break;
      }
      case "Escape": {
        event.preventDefault();
        handleOpenChange(false);
        break;
      }
    }
  }

  // Keep the highlighted row within bounds and on an enabled option whenever the
  // list's CONTENT changes — the query narrowing it, or asynchronous model
  // discovery landing. Deliberately keyed on `signature` rather than on the
  // array, so a parent re-render does not move the user's highlight.
  React.useEffect(() => {
    setHighlightedIndex(firstEnabled);
    // eslint-disable-next-line react-hooks/exhaustive-deps -- content, not identity
  }, [signature]);

  // The search box's element, focused from `onOpenAutoFocus` below.
  //
  // Focus has to happen at Radix's HANDOVER point, not when the input mounts.
  // `onOpenAutoFocus` + `preventDefault` means "do not move focus into the
  // content", which leaves it on the TRIGGER — so a mount-time callback ref
  // focusing the input runs first and then loses focus again when Radix's focus
  // scope settles. Measured: typing straight after opening did not filter the
  // list at all, arrow keys did nothing because the key handler is on this input,
  // and Enter re-toggled the still-focused trigger. The original code hid this by
  // re-focusing on every render, which won the race by repetition and is the same
  // churn that let typing land in the instructions field.
  const searchRef = React.useRef<HTMLInputElement | null>(null);

  return (
    <div className={PERSONA_FIELD_SHELL_CLASS}>
      {/* `modal={false}` deliberately: Radix's modal mode traps focus inside the
          popover, which breaks the search box and the arrow keys. The reason this
          popover still takes clicks inside a modal Dialog is
          `pointer-events-auto` on its content below — see the comment there. */}
      <Popover modal={false} onOpenChange={handleOpenChange} open={open}>
        <PopoverTrigger asChild>
          <button
            aria-expanded={open}
            className={cn(
              "flex h-11 w-full items-center justify-between gap-3 px-3 py-2 text-left text-sm leading-6",
              PERSONA_FIELD_CONTROL_CLASS,
              disabled && "cursor-default opacity-60",
            )}
            disabled={disabled}
            id={id}
            role="combobox"
            type="button"
          >
            <span
              className={cn(
                "min-w-0 flex-1 truncate",
                !selectedOption && "text-muted-foreground/55",
              )}
            >
              {selectedOption?.label ?? placeholder}
            </span>
            <ChevronDown className="h-4 w-4 shrink-0 text-muted-foreground/60" />
          </button>
        </PopoverTrigger>
        <PopoverContent
          align="start"
          // `pointer-events-auto` is what makes the rows CLICKABLE, and `z-[60]`
          // is what makes them VISIBLE above the dialog. Two different problems,
          // and only the first one was the mystery.
          //
          // A modal Radix Dialog sets `pointer-events: none` on the BODY while it
          // is open and re-enables it on its own content. This popover portals to
          // the body, so it inherits `none`: a click on a model row was never
          // dispatched to the row at all, and the topmost element under the pointer
          // was reported as the dialog overlay. Keyboard selection was unaffected
          // because it involves no hit-testing, which is what made this look like a
          // mis-click rather than a bug.
          //
          // The z-index is raised here rather than in `shared/ui/popover.tsx`:
          // every other popover in the app opens outside a dialog, and lifting them
          // all would put them above overlays they are correctly beneath.
          className="pointer-events-auto z-[60] overflow-hidden p-0"
          onOpenAutoFocus={(event) => {
            // Radix would focus the first focusable in the content; take that
            // over and put focus on the search box instead. Doing it HERE rather
            // than on mount is what makes it stick (see `searchRef`).
            event.preventDefault();
            searchRef.current?.focus();
          }}
          sideOffset={5}
          style={{
            minWidth: "var(--radix-popover-trigger-width)",
            width: "var(--radix-popover-trigger-width)",
          }}
        >
          <div className="group/search flex cursor-text items-center gap-2 border-b border-border/50 px-3 py-2">
            <Search className="h-3.5 w-3.5 shrink-0 text-muted-foreground/55 transition-colors duration-150 ease-out group-focus-within/search:text-foreground" />
            <input
              aria-label="Search models"
              autoCapitalize="none"
              autoComplete="off"
              autoCorrect="off"
              className="block min-w-0 flex-1 border-0 bg-transparent p-0 text-sm leading-5 text-muted-foreground/55 shadow-none outline-none placeholder:text-muted-foreground/55 focus:text-foreground focus:placeholder:text-muted-foreground"
              onChange={(event) => setQuery(event.target.value)}
              onKeyDown={handleKeyDown}
              placeholder="Search models…"
              ref={searchRef}
              spellCheck={false}
              value={query}
            />
          </div>
          <div
            className="max-h-[min(16rem,var(--radix-popover-content-available-height))] overflow-y-auto overscroll-contain p-1"
            onTouchMoveCapture={(event) => event.stopPropagation()}
            onWheelCapture={(event) => event.stopPropagation()}
          >
            {filteredOptions.length > 0 ? (
              filteredOptions.map((option, index) => (
                <button
                  aria-disabled={option.disabled}
                  className={cn(
                    "relative flex min-h-9 w-full select-none items-center rounded-lg py-2 pl-8 pr-4 text-left text-sm outline-none transition-colors",
                    option.disabled
                      ? "pointer-events-none opacity-50"
                      : "cursor-default hover:bg-muted/50 hover:text-foreground",
                    index === highlightedIndex &&
                      !option.disabled &&
                      "bg-muted/50 text-foreground",
                    option.value === value && "font-medium",
                  )}
                  disabled={option.disabled}
                  key={option.value}
                  onClick={() => selectOption(option.value)}
                  onMouseEnter={() => {
                    if (!option.disabled) setHighlightedIndex(index);
                  }}
                  type="button"
                >
                  <span className="absolute left-2 flex h-3.5 w-3.5 items-center justify-center">
                    <Check
                      className={cn(
                        "h-3.5 w-3.5",
                        option.value === value ? "opacity-100" : "opacity-0",
                      )}
                    />
                  </span>
                  <span className="truncate">{option.label}</span>
                </button>
              ))
            ) : (
              <p className="px-3 py-6 text-center text-sm text-muted-foreground/55">
                No models match
              </p>
            )}
          </div>
        </PopoverContent>
      </Popover>
    </div>
  );
}
