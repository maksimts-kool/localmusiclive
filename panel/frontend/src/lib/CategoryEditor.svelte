<script>
  import Icon from './Icon.svelte';

  // Toggle existing categories or type a new one. `selected` is the current
  // list; onToggle(name) adds or removes one; onDone closes the editor.
  let { all, selected, onToggle, onDone, placeholder = 'New category…' } = $props();

  let draft = $state('');
  let input;

  let names = $derived.by(() => {
    const seen = new Set(all.map((c) => c.name.toLowerCase()));
    return [...all.map((c) => c.name), ...selected.filter((n) => !seen.has(n.toLowerCase()))];
  });
  let chosen = $derived(new Set(selected.map((n) => n.toLowerCase())));

  $effect(() => input?.focus());

  function submit(event) {
    event.preventDefault();
    const name = draft.trim();
    if (!name) return onDone();
    if (!chosen.has(name.toLowerCase())) onToggle(name);
    draft = '';
  }

  function keydown(event) {
    if (event.key === 'Escape') onDone();
  }
</script>

<div class="editor">
  <div class="chips">
    {#each names as name (name)}
      <button
        type="button"
        class="chip"
        class:on={chosen.has(name.toLowerCase())}
        aria-pressed={chosen.has(name.toLowerCase())}
        onclick={() => onToggle(name)}
      >
        {#if chosen.has(name.toLowerCase())}<Icon name="check" size={12} />{/if}{name}
      </button>
    {/each}
  </div>
  <form class="new" onsubmit={submit}>
    <input type="text" maxlength="32" {placeholder} bind:value={draft} bind:this={input} onkeydown={keydown} />
    <button class="btn" type="submit">{draft.trim() ? 'Add' : 'Done'}</button>
  </form>
</div>

<style>
  .editor {
    display: flex;
    flex-direction: column;
    gap: 8px;
    padding: 8px 10px 10px;
    margin: 0 4px 6px;
    border-radius: 8px;
    background: var(--surface-2);
    border: 1px solid var(--border);
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
  }

  .chip {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    height: 26px;
    padding: 0 10px;
    border-radius: 99px;
    border: 1px solid var(--border);
    background: var(--surface);
    color: var(--muted);
    font-size: 12.5px;
  }

  .chip:hover {
    color: var(--text);
  }

  .chip.on {
    border-color: transparent;
    background: color-mix(in srgb, var(--accent) 20%, transparent);
    color: var(--accent);
    font-weight: 600;
  }

  .new {
    display: flex;
    gap: 8px;
  }

  .new input {
    flex: 1;
  }
</style>
