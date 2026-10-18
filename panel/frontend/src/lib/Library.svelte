<script>
  import Icon from './Icon.svelte';
  import { api } from './api.js';
  import { attempt, toast } from './toast.svelte.js';
  import { duration } from './format.js';

  let { version, queuedIds, scanning, size } = $props();

  const PAGE = 60;
  let query = $state('');
  let folder = $state('');
  let sort = $state('artist');
  let folders = $state([]);
  let items = $state([]);
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
    const deps = [query, folder, sort, version];
    const timer = setTimeout(() => load(true), deps[0] ? 200 : 0);
    return () => clearTimeout(timer);
  });

  $effect(() => {
    version;
    api.get('/api/library/folders').then((f) => (folders = f)).catch(() => {});
  });

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
  <select bind:value={sort} aria-label="Sort">
    <option value="artist">Artist</option>
    <option value="title">Title</option>
    <option value="added">Recently added</option>
    <option value="recent">Recently played</option>
    <option value="played">Most played</option>
  </select>
</div>

<ul class="list">
  {#each items as track (track.id)}
    <li class="row" ondblclick={() => add(track, false)}>
      <div class="thumb small" style:background-image={track.thumb ? `url(${track.thumb})` : null}>
        {#if !track.thumb}<Icon name="music" size={16} />{/if}
      </div>
      <div class="text">
        <div class="ellipsis title" title={track.path}>{track.title}</div>
        <div class="ellipsis muted sub">
          {track.artist || track.folder || ' '}
        </div>
      </div>
      {#if queuedIds.has(track.id)}<span class="badge queue">Queued</span>{/if}
      <span class="muted dur">{duration(track.duration)}</span>
      <button class="icon-btn" title="Play next" onclick={() => add(track, true)}>
        <Icon name="next" size={17} />
      </button>
      <button class="icon-btn add" title="Add to queue" onclick={() => add(track, false)}>
        <Icon name="plus" size={18} />
      </button>
    </li>
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
