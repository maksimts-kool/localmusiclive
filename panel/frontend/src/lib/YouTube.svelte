<script>
  import Icon from './Icon.svelte';
  import { api } from './api.js';
  import { attempt, toast } from './toast.svelte.js';
  import { duration } from './format.js';

  let { downloads } = $props();

  let query = $state('');
  let results = $state([]);
  let searched = $state('');
  let searching = $state(false);

  // New songs to download, picked from what plays most on the stream.
  let suggested = $state(null);
  let suggesting = $state(false);

  async function suggest(refresh = false) {
    suggesting = true;
    try {
      suggested = await api.get(`/api/youtube/suggestions${refresh ? '?refresh=true' : ''}`);
    } catch (error) {
      toast(error.message, 'error');
    } finally {
      suggesting = false;
    }
  }

  $effect(() => {
    suggest();
  });

  // Clearing the search box goes back to the suggestions.
  $effect(() => {
    if (!query.trim()) {
      results = [];
      searched = '';
    }
  });

  let basedOn = $derived.by(() => {
    const names = (suggested?.based_on ?? []).slice(0, 3).map((t) => t.title);
    const more = (suggested?.based_on?.length ?? 0) - names.length;
    return names.join(', ') + (more > 0 ? ` and ${more} more` : '');
  });

  // Downloads by video id, to show progress on the matching result.
  let active = $derived(new Map(downloads.map((d) => [d.video_id, d])));

  async function search(event) {
    event?.preventDefault();
    const q = query.trim();
    if (!q) return;
    searching = true;
    try {
      results = await api.get(`/api/youtube/search?q=${encodeURIComponent(q)}`);
      searched = q;
    } catch (error) {
      toast(error.message, 'error');
    } finally {
      searching = false;
    }
  }

  function download(video, then) {
    const verb = { none: 'Downloading', queue: 'Downloading, then queueing', next: 'Downloading, then playing next' }[then];
    attempt(() => api.post('/api/youtube/downloads', { video_id: video.id, then, title: video.title }), `${verb}: ${video.title}`);
  }

  function views(n) {
    if (n == null) return '';
    if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B views`;
    if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M views`;
    if (n >= 1e3) return `${Math.round(n / 1e3)}K views`;
    return `${n} views`;
  }

</script>

<form class="searchbar" onsubmit={search}>
  <label class="search">
    <span class="sr-only">Search YouTube</span>
    <Icon name="search" size={16} />
    <input type="search" placeholder="Search YouTube or paste a link…" bind:value={query} />
  </label>
  <button class="btn primary" disabled={searching || !query.trim()}>{searching ? 'Searching…' : 'Search'}</button>
</form>

{#snippet item(video)}
  {@const job = active.get(video.id)}
  {@const busy = job && ['waiting', 'downloading', 'adding'].includes(job.status)}
  <li class="row">
    <a class="thumb yt" href="https://www.youtube.com/watch?v={video.id}" target="_blank" rel="noreferrer"
      style:background-image="url({video.thumb})" title="Open on YouTube">
      {#if video.duration}<span class="len">{duration(video.duration)}</span>{/if}
    </a>
    <div class="text">
      <div class="title clamp" title={video.title}>{video.title}</div>
      <div class="ellipsis muted sub">{video.channel}{video.views != null ? ` · ${views(video.views)}` : ''}</div>
      {#if video.reasons?.length}
        <div class="reason ellipsis">{video.reasons.join(' · ')}</div>
      {/if}
      <div class="labels">
        {#if video.track_id || job?.status === 'done'}<span class="badge queue">In library</span>{/if}
        {#if busy}<span class="badge queue">{job.status === 'downloading' ? `${Math.round(job.progress * 100)}%` : 'Downloading'}</span>{/if}
      </div>
    </div>
    <div class="actions">
      <button class="icon-btn" title="Download only" disabled={busy} onclick={() => download(video, 'none')}>
        <Icon name="download" size={17} />
      </button>
      <button class="icon-btn" title="Download and play next" disabled={busy} onclick={() => download(video, 'next')}>
        <Icon name="next" size={17} />
      </button>
      <button class="icon-btn add" title="Download and add to queue" disabled={busy} onclick={() => download(video, 'queue')}>
        <Icon name="plus" size={18} />
      </button>
    </div>
  </li>
{/snippet}

{#if searched || searching}
  <ul class="list">
    {#each results as video (video.id)}{@render item(video)}{/each}
  </ul>
  {#if searched && !searching && !results.length}
    <div class="empty">No videos found for “{searched}”.</div>
  {/if}
{:else}
  <div class="head">
    <div>
      <h2 class="section-title"><Icon name="sparkles" size={13} /> Suggested for you</h2>
      {#if basedOn}<div class="muted based">From your most played: {basedOn}</div>{/if}
    </div>
    <button class="icon-btn" title="New suggestions" onclick={() => suggest(true)} disabled={suggesting}>
      <Icon name={suggesting ? 'loader' : 'refresh'} size={16} />
    </button>
  </div>
  {#if suggested?.items.length}
    <ul class="list">
      {#each suggested.items as video (video.id)}{@render item(video)}{/each}
    </ul>
  {:else if suggesting}
    <div class="empty">Looking at your most played songs on YouTube…</div>
  {:else if suggested}
    <div class="empty">
      {#if suggested.based_on.length}
        No new songs found. Try again later, or search below.
      {:else}
        Suggestions appear once your library has songs.
      {/if}
    </div>
  {/if}
  <div class="empty hint">
    Search YouTube for a music video, or paste a link. Downloads go to
    <code>media/youtube/</code> and join the library; <b>+</b> also queues the song.
  </div>
{/if}

<style>
  .searchbar {
    display: flex;
    gap: 8px;
    margin-bottom: 10px;
  }

  .search {
    flex: 1;
    display: flex;
    align-items: center;
    gap: 6px;
    padding-left: 10px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--surface-2);
    color: var(--muted);
    min-width: 0;
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

  .head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 8px;
    margin: 2px 0 6px;
  }

  .head .section-title {
    display: flex;
    align-items: center;
    gap: 5px;
  }

  .based {
    font-size: 12px;
    margin-top: 2px;
  }

  .reason {
    font-size: 12px;
    color: var(--accent);
  }

  .hint {
    padding-top: 16px;
    font-size: 12.5px;
  }

  .labels {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-top: 4px;
  }

  .labels:empty {
    display: none;
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

  .thumb.yt {
    position: relative;
    width: 120px;
  }

  .len {
    position: absolute;
    right: 4px;
    bottom: 4px;
    padding: 0 4px;
    border-radius: 4px;
    background: rgb(0 0 0 / 0.75);
    color: #fff;
    font-size: 11px;
    font-variant-numeric: tabular-nums;
  }

  .text {
    flex: 1;
    min-width: 0;
  }

  .title {
    font-weight: 550;
  }

  .clamp {
    display: -webkit-box;
    -webkit-line-clamp: 2;
    line-clamp: 2;
    -webkit-box-orient: vertical;
    overflow: hidden;
  }

  .sub {
    font-size: 12.5px;
  }

  .actions {
    display: flex;
    flex: none;
  }

  .add {
    color: var(--accent);
  }

  code {
    font-size: 12.5px;
  }
</style>
