import * as React from "react";

/**
 * State that is read from, and persisted to, storage under one account's key.
 *
 * The value is held together with the pubkey it was read for. A plain
 * read-effect/write-effect pair keyed on the pubkey is not safe: when the
 * signed-in pubkey changes, both effects run in the same commit, the read only
 * QUEUES the new account's value, and the write then persists the previous
 * account's value under the new account's key. Here a value is only ever
 * written under the pubkey it belongs to, and a render that happens before the
 * new account's value has been adopted shows that account's stored value rather
 * than the previous account's.
 *
 * `read` and `write` must be stable (module-level functions); an empty pubkey
 * means "no account" and `write` is expected to ignore it.
 */
export function usePubkeyScopedStoredState<T>(
  pubkey: string,
  read: (pubkey: string) => T,
  write: (pubkey: string, value: T) => void,
): [T, (update: React.SetStateAction<T>) => void] {
  const [owned, setOwned] = React.useState(() => ({
    owner: pubkey,
    value: read(pubkey),
  }));
  const ownerRef = React.useRef(pubkey);
  ownerRef.current = pubkey;

  // Adopt the new account's stored value once per pubkey change. Until this
  // lands, `value` below reads it straight from storage.
  React.useEffect(() => {
    setOwned((current) =>
      current.owner === pubkey ? current : { owner: pubkey, value: read(pubkey) },
    );
  }, [pubkey, read]);

  React.useEffect(() => {
    // Never persist a value under an account it was not read for.
    if (owned.owner !== pubkey) return;
    write(owned.owner, owned.value);
  }, [owned, pubkey, write]);

  const setValue = React.useCallback(
    (update: React.SetStateAction<T>) => {
      setOwned((current) => {
        const owner = ownerRef.current;
        const base = current.owner === owner ? current.value : read(owner);
        const value =
          typeof update === "function"
            ? (update as (previous: T) => T)(base)
            : update;
        return current.owner === owner && Object.is(value, current.value)
          ? current
          : { owner, value };
      });
    },
    [read],
  );

  const value = owned.owner === pubkey ? owned.value : read(pubkey);
  return [value, setValue];
}
