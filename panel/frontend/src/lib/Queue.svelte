<script>
  import Sortable from 'sortablejs';
  import Icon from './Icon.svelte';
  import { api } from './api.js';
  import { attempt } from './toast.svelte.js';
  import { duration, eta } from './format.js';

  let { queue, upcoming, autoplay } = $props();

  // Local copy so a drag in progress isn't redrawn by incoming updates.
  let items = $state.raw([]);
  let dragging = false;
  let listEl;

  $effect(() => {
    const latest = queue;
    if (!dragging) items = latest;
  });

  $effect(() => {
    const sortable = Sortable.create(listEl, {
      handle: '.grip',
      animation: 150,
      ghostClass: 'ghost',
      onStart: () => (dragging = true),
      onEnd: (event) => {
        dragging = false;
        const { oldIndex, newIndex, item, from } = event;
        if (oldIndex === newIndex) return;
        // Put the node back where Svelte expects it; Svelte redraws the new order.
        from.removeChild(item);
        from.insertBefore(item, from.children[oldIndex] ?? null);
        const next = [...items];
        next.splice(newIndex, 0, next.splice(oldIndex, 1)[0]);
        items = next;
        attempt(() => api.put('/api/queue', { ids: next.map((q) => q.id) }));
      },
    });
    return () => sortable.destroy();
  });

  function remove(id) {
    items = items.filter((q) => q.id !== id);
    attempt(() => api.del(`/api/queue/${id}`));
  }

  function clear() {
    if (confirm('Remove everything from the queue?')) attempt(() => api.del('/api/queue'));
  }
</script>

<div class="head">
  <h2 class="section-title">Queue <span class="count">{items.length}</span></h2>
  {#if items.length}
    <button class="btn" onclick={clear}>Clear</button>
  {/if}
</div>

<ol class="list" bind:this={listEl}>
  {#each items as q (q.id)}
    <li class="row">
      <span class="grip" title="Drag to reorder"><Icon name="grip" size={16} /></span>
      <div class="thumb small" style:background-image={q.track?.thumb ? `url(${q.track.thumb})` : null}></div>
      <div class="text">
        <div class="ellipsis title">{q.track?.title ?? 'Missing track'}</div>
        <div class="ellipsis muted sub">
          {q.track?.artist}
          {#if q.requested_by}· requested by {q.requested_by}{/if}
        </div>
      </div>
      <div class="meta">
        <div class="eta">{eta(q.eta)}</div>
        <div class="muted">{duration(q.track?.duration)}</div>
      </div>
      <button class="icon-btn" title="Remove from queue" onclick={() => remove(q.id)}>
        <Icon name="trash" size={16} />
      </button>
    </li>
  {/each}
</ol>

{#if !items.length}
  <div class="empty">
    The queue is empty.
    {#if autoplay?.enabled}Autoplay keeps the stream going.{:else}Autoplay is off, so the stream will go black.{/if}
  </div>
{/if}

{#if autoplay?.enabled && upcoming?.length}
  <h3 class="section-title then">Then autoplay</h3>
  <ol class="list auto">
    {#each upcoming.slice(0, 6) as u (u.id)}
      <li class="row">
        <div class="thumb small" style:background-image={u.track?.thumb ? `url(${u.track.thumb})` : null}></div>
        <div class="text">
          <div class="ellipsis title">{u.track?.title}</div>
          <div class="ellipsis muted sub">{u.track?.artist}</div>
        </div>
        <div class="meta">
          <div class="eta">{eta(u.eta)}</div>
          <div class="muted">{duration(u.track?.duration)}</div>
        </div>
      </li>
    {/each}
  </ol>
{/if}

<style>
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    min-height: 34px;
    margin-bottom: 6px;
  }

  .count {
    margin-left: 4px;
    color: var(--faint);
  }

  .list {
    list-style: none;
    margin: 0;
    padding: 0;
  }

  .row {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 6px 4px;
    border-radius: 8px;
    min-width: 0;
  }

  .row:hover {
    background: var(--surface-2);
  }

  .grip {
    display: grid;
    place-items: center;
    width: 20px;
    color: var(--faint);
    cursor: grab;
    touch-action: none;
  }

  .thumb.small {
    width: 64px;
  }

  .text {
    flex: 1;
    min-width: 0;
  }

  .title {
    font-weight: 550;
  }

  .sub {
    font-size: 12.5px;
  }

  .meta {
    text-align: right;
    font-size: 12px;
    font-variant-numeric: tabular-nums;
    flex: none;
  }

  .eta {
    color: var(--text);
  }

  .then {
    margin: 18px 0 6px;
  }

  .auto .row {
    opacity: 0.72;
  }

  :global(.ghost) {
    opacity: 0.35;
    background: var(--surface-3);
  }
</style>
