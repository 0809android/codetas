function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function sameValue(left: unknown, right: unknown): boolean {
  if (Array.isArray(left) && Array.isArray(right)) {
    return left.length === right.length && left.every((item, index) => sameValue(item, right[index]));
  }
  if (isPlainObject(left) && isPlainObject(right)) {
    const keys = new Set([...Object.keys(left), ...Object.keys(right)]);
    return [...keys].every((key) => sameValue(left[key], right[key]));
  }
  return left === right;
}

/** Connections by `id`, accounts by provider and `id`. */
function itemKey(item: unknown): string | undefined {
  if (!isPlainObject(item) || typeof item.id !== "string") return undefined;
  return JSON.stringify([typeof item.providerId === "string" ? item.providerId : null, item.id]);
}

/** Null unless every item has a key no other item in the array shares. */
function keyedItems(items: unknown[]): Map<string, unknown> | null {
  const keyed = new Map<string, unknown>();
  for (const item of items) {
    const key = itemKey(item);
    if (key === undefined || keyed.has(key)) return null;
    keyed.set(key, item);
  }
  return keyed;
}

/**
 * A list of connections or accounts merges item by item, so that editing one
 * connection keeps one that startup adopted meanwhile. The user's removals and
 * additions win; an item removed elsewhere stays removed unless the user edited it.
 */
function mergeKeyedArrays(base: unknown[], mine: unknown[], latest: unknown[]): unknown[] | null {
  const [baseItems, mineItems, latestItems] = [keyedItems(base), keyedItems(mine), keyedItems(latest)];
  if (!baseItems || !mineItems || !latestItems) return null;
  const merged: unknown[] = [];
  for (const [key, item] of mineItems) {
    if (!baseItems.has(key)) {
      merged.push(item);
    } else if (latestItems.has(key)) {
      merged.push(mergeConfigurationChanges(baseItems.get(key), item, latestItems.get(key)));
    } else if (!sameValue(baseItems.get(key), item)) {
      merged.push(item);
    }
  }
  for (const [key, item] of latestItems) {
    if (!baseItems.has(key) && !mineItems.has(key)) merged.push(item);
  }
  return merged;
}

/**
 * The window's copy of the settings can be behind the file: the tray disconnects,
 * startup adopts a connection. A save writes the whole file, so it takes `latest` and
 * applies only what the user changed from `base`, the copy the form was drawn from.
 * Objects merge field by field, and lists of items with an `id` item by item.
 * Any other array is one value.
 */
export function mergeConfigurationChanges<T>(base: T, mine: T, latest: T): T {
  if (sameValue(base, mine)) return latest;
  if (Array.isArray(base) && Array.isArray(mine) && Array.isArray(latest)) {
    return (mergeKeyedArrays(base, mine, latest) ?? mine) as T;
  }
  if (!isPlainObject(base) || !isPlainObject(mine) || !isPlainObject(latest)) return mine;
  const merged: Record<string, unknown> = {};
  for (const key of new Set([...Object.keys(base), ...Object.keys(mine), ...Object.keys(latest)])) {
    const value = mergeConfigurationChanges(base[key], mine[key], latest[key]);
    if (value !== undefined) merged[key] = value;
  }
  return merged as T;
}
