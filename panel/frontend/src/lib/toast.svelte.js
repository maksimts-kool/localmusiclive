let nextId = 1;

export const toasts = $state([]);

export function toast(message, kind = 'info') {
  const id = nextId++;
  toasts.push({ id, message, kind });
  setTimeout(() => {
    const index = toasts.findIndex((t) => t.id === id);
    if (index >= 0) toasts.splice(index, 1);
  }, kind === 'error' ? 6000 : 2500);
}

/** Run an API call and show its error, if any, as a toast. */
export async function attempt(action, success) {
  try {
    const result = await action();
    if (success) toast(success);
    return result;
  } catch (error) {
    toast(error.message, 'error');
    return undefined;
  }
}
