<script>
  import Icon from './Icon.svelte';
  import CategoryEditor from './CategoryEditor.svelte';
  import { api } from './api.js';
  import { attempt, toast } from './toast.svelte.js';
  import { duration } from './format.js';

  let { version, queuedIds, scanning, size } = $props();

  const PAGE = 60;
  let query = $state('');
  let folder = $state('');
  let category = $state('');
  let categories = $state([]);
  // Track id whose categories are being edited, or 'bulk' for every match.
  let editing = $state(null);
  let sort = $state('artist');
  let folders = $state([]);
  let items = $state.raw([]);
  let total = $state(0);
  let loading = $state(false);
  let sentinel;
  let request = 0;

  async function load(reset) {
    const id = ++request;
    loading = true;
    const params = new URLSearchParams({
      q: query,
      folder,
      category,
      sort,
      limit: PAGE,
      offset: reset ? 0 : items.length,
    });
    try {
      const page = await api.get(`/api/library?${params}`);
      if (id !== request) return;
      items = reset ? page.items : [...items, ...page.items];
      total = page.total;
    } catch (error) {
      toast(error.message, 'error');
    } finally {
      if (id === request) loading = false;
    }
  }

  // Reload when the filters change (search is debounced) or the library is rescanned.
  $effect(() => {
    const deps = [query, folder, category, sort, version];
    const timer = setTimeout(() => load(true), deps[0] ? 200 : 0);
    return () => clearTimeout(timer);
  });

  $effect(() => {
    version;
    api.get('/api/library/folders').then((f) => (folders = f)).catch(() => {});
    loadCategories();
  });

  function loadCategories() {
    return api.get('/api/categories').then((c) => (categories = c)).catch(() => {});
  }

  let filtered = $derived(Boolean(query.trim() || folder || category));

  // Saves run one after another, so quick clicks never send a stale list.
  let saving = Promise.resolve();

  function setCategories(id, categories) {
    items = items.map((t) => (t.id === id ? { ...t, categories } : t));
  }

  function toggle(track, name) {
    // Start from the latest list (the row may have changed since it was drawn).
    const current = items.find((t) => t.id === track.id)?.categories ?? track.categories;
    const key = name.toLowerCase();
    const next = current.some((c) => c.toLowerCase() === key)
      ? current.filter((c) => c.toLowerCase() !== key)
      : [...current, name];
    setCategories(track.id, next);
    saving = saving.then(async () => {
      try {
        const saved = await api.put(`/api/tracks/${track.id}/categories`, { categories: next });
        // Only take the server's spelling if nothing newer was picked meanwhile.
        const now = items.find((t) => t.id === track.id)?.categories;
        if (now === next) setCategories(track.id, saved.categories);
        loadCategories();
      } catch (error) {
        toast(error.message, 'error');
      }
    });
  }

  async function bulk(change) {
    const filter = { q: query, folder, category };
    try {
      const { changed } = await api.post('/api/categories/bulk', { ...filter, ...change });
      const name = change.add ?? change.remove;
      toast(changed ? `${change.add ? 'Added' : 'Removed'} “${name}” on ${changed} song${changed === 1 ? '' : 's'}` : 'Nothing to change');
      if (change.remove) editing = null;
    } catch (error) {
      toast(error.message, 'error');
    }
  }

  $effect(() => {
    const observer = new IntersectionObserver((entries) => {
      if (entries[0].isIntersecting && !loading && items.length < total) load(false);
    });
    observer.observe(sentinel);
    return () => observer.disconnect();
  });

  function add(track, playNext) {
    attempt(
      () => api.post('/api/queue', { track_id: track.id, play_next: playNext }),
      playNext ? `Playing next: ${track.title}` : `Queued: ${track.title}`,
    );
  }

  async function remove(track) {
    const name = track.artist ? `${track.artist} – ${track.title}` : track.title;
    if (!confirm(`Delete “${name}”?\n\nThe video file is deleted from the media folder for good.`)) return;
    try {
      await api.del(`/api/tracks/${track.id}`);
      items = items.filter((t) => t.id !== track.id);
      total -= 1;
      if (editing === track.id) editing = null;
      toast(`Deleted ${name}`);
      loadCategories();
    } catch (error) {
      toast(error.message, 'error');
    }
  }

  function rescan() {
    attempt(() => api.post('/api/library/rescan'), 'Scanning the media folder…');
  }
</script>

<div class="head">
  <h2 class="section-title">Library <span class="count">{size}</span></h2>
  <button class="icon-btn" title="Rescan media folder" onclick={rescan} disabled={scanning}>
    <span class:spin={scanning}><Icon name="refresh" size={16} /></span>
  </button>
</div>

<div class="filters">
  <label class="search">
    <span class="sr-only">Search</span>
    <Icon name="search" size={16} />
    <input type="search" placeholder="Search artist, title, file…" bind:value={query} />
  </label>
  <select bind:value={folder} aria-label="Folder">
    <option value="">All folders</option>
    {#each folders as f}<option value={f}>{f}</option>{/each}
  </select>
  <select bind:value={category} aria-label="Category">
    <option value="">All categories</option>
    {#each categories as c}<option value={c.name}>{c.name} ({c.count})</option>{/each}
    <option value="__none__">No category</option>
  </select>
  <select bind:value={sort} aria-label="Sort">
    <option value="artist">Artist</option>
    <option value="title">Title</option>
    <option value="added">Recently added</option>
    <option value="recent">Recently played</option>
    <option value="played">Most played</option>
  </select>
</div>

{#if filtered && total}
  <div class="bulk">
    <span class="muted">{total} song{total === 1 ? '' : 's'} shown</span>
    {#if category && category !== '__none__'}
      <button class="btn small" onclick={() => confirm(`Remove “${category}” from ${total} songs?`) && bulk({ remove: category })}>
        Remove “{category}”
      </button>
    {/if}
    <button class="btn small" class:active={editing === 'bulk'} onclick={() => (editing = editing === 'bulk' ? null : 'bulk')}>
      <Icon name="tag" size={14} /> Tag all
    </button>
  </div>
  {#if editing === 'bulk'}
    <CategoryEditor
      all={categories}
      selected={[]}
      placeholder="Category to add to all {total}…"
      onToggle={(name) => bulk({ add: name })}
      onDone={() => (editing = null)}
    />
  {/if}
{/if}

<ul class="list">
  {#each items as track (track.id)}
    <li class="row" ondblclick={() => add(track, false)}>
      <div class="thumb small" style:background-image={track.thumb ? `url(${track.thumb})` : null}>
        {#if !track.thumb}<Icon name="music" size={16} />{/if}
      </div>
      <div class="text">
        <div class="title-line">
          <span class="ellipsis title" title={track.path}>{track.title}</span>
          {#if track.play_count}
            <span class="plays" title="Played {track.play_count} time{track.play_count === 1 ? '' : 's'} on the stream">
              <Icon name="play" size={9} />{track.play_count}
            </span>
          {/if}
        </div>
        <div class="ellipsis muted sub">
          {track.artist || track.folder || ' '}
        </div>
        {#if track.categories?.length}
          <div class="cats">
            {#each track.categories as c}
              <button class="cat" title="Show only {c}" onclick={() => (category = c)}>{c}</button>
            {/each}
          </div>
        {/if}
      </div>
      {#if queuedIds.has(track.id)}<span class="badge queue">Queued</span>{/if}
      <span class="muted dur">{duration(track.duration)}</span>
      <button
        class="icon-btn"
        class:active={editing === track.id}
        title="Categories"
        onclick={() => (editing = editing === track.id ? null : track.id)}
      >
        <Icon name="tag" size={16} />
      </button>
      <button class="icon-btn" title="Play next" onclick={() => add(track, true)}>
        <Icon name="next" size={17} />
      </button>
      <button class="icon-btn add" title="Add to queue" onclick={() => add(track, false)}>
        <Icon name="plus" size={18} />
      </button>
      <button class="icon-btn danger" title="Delete song" onclick={() => remove(track)}>
        <Icon name="trash" size={16} />
      </button>
    </li>
    {#if editing === track.id}
      <li>
        <CategoryEditor
          all={categories}
          selected={track.categories}
          onToggle={(name) => toggle(track, name)}
          onDone={() => (editing = null)}
        />
      </li>
    {/if}
  {/each}
</ul>
<div bind:this={sentinel} class="sentinel"></div>

{#if !loading && !items.length}
  <div class="empty">
    {#if size === 0}
      No clips yet. Put video files in your media folder; they show up here within a few minutes,
      or press rescan.
    {:else}
      Nothing matches.
    {/if}
  </div>
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

  .filters {
    display: flex;
    gap: 8px;
    margin-bottom: 8px;
    flex-wrap: wrap;
  }

  .search {
    flex: 1 1 220px;
    display: flex;
    align-items: center;
    gap: 6px;
    padding-left: 10px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--surface-2);
    color: var(--muted);
  }

  .search:focus-within {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }

  .search input {
    flex: 1;
    border: 0;
    background: transparent;
    padding-left: 0;
    outline: none;
    color: var(--text);
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
    padding: 5px 4px;
    border-radius: 8px;
    min-width: 0;
  }

  .row:hover {
    background: var(--surface-2);
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

  .title-line {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }

  .plays {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    flex: none;
    height: 17px;
    padding: 0 6px;
    border-radius: 99px;
    background: color-mix(in srgb, var(--accent) 16%, transparent);
    color: var(--accent);
    font-size: 11px;
    font-weight: 700;
    font-variant-numeric: tabular-nums;
  }

  .sub {
    font-size: 12.5px;
  }

  .dur {
    font-size: 12px;
    font-variant-numeric: tabular-nums;
    flex: none;
  }

  .add {
    color: var(--accent);
  }

  .icon-btn.danger:hover:not(:disabled) {
    color: var(--live);
  }

  .icon-btn.active {
    background: var(--surface-3);
    color: var(--accent);
  }

  .cats {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-top: 3px;
  }

  .cat {
    height: 18px;
    padding: 0 7px;
    border: 0;
    border-radius: 99px;
    background: var(--surface-3);
    color: var(--muted);
    font-size: 11px;
    font-weight: 600;
  }

  .cat:hover {
    color: var(--accent);
  }

  .bulk {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
    margin: -2px 0 8px;
    font-size: 12.5px;
  }

  .bulk .muted {
    margin-right: auto;
  }

  .btn.small {
    height: 28px;
    padding: 0 10px;
    font-size: 12.5px;
  }

  .btn.active {
    border-color: var(--accent);
    color: var(--accent);
  }

  .sentinel {
    height: 1px;
  }

  .spin {
    display: inline-grid;
    animation: spin 1s linear infinite;
  }

  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
