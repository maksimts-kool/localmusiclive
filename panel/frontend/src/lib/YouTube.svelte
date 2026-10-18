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
    attempt(() => api.post('/api/youtube/downloads', { video_id: video.id, then }), `${verb}: ${video.title}`);
  }

  function dismiss(job) {
    attempt(() => api.del(`/api/youtube/downloads/${job.id}`));
  }

  function views(n) {
    if (n == null) return '';
    if (n >= 1e9) return `${(n / 1e9).toFixed(1)}B views`;
    if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M views`;
    if (n >= 1e3) return `${Math.round(n / 1e3)}K views`;
    return `${n} views`;
  }

  const statusText = {
    waiting: 'Waiting…',
    downloading: 'Downloading',
    adding: 'Adding to library…',
    done: 'Added',
    error: 'Failed',
  };

  function jobLine(job) {
    if (job.status === 'downloading') return `Downloading ${Math.round(job.progress * 100)}%`;
    if (job.status === 'done') return { queue: 'Added and queued', next: 'Added, plays next', none: 'Added to library' }[job.then];
    if (job.status === 'error') return job.error;
    return statusText[job.status];
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

{#if downloads.length}
  <div class="jobs">
    {#each downloads as job (job.id)}
      <div class="job" class:error={job.status === 'error'}>
        <div class="thumb tiny" style:background-image="url({job.thumb})"></div>
        <div class="text">
          <div class="ellipsis title">{job.title}</div>
          <div class="ellipsis sub" class:muted={job.status !== 'error'}>{jobLine(job)}</div>
          {#if job.status === 'downloading' || job.status === 'waiting' || job.status === 'adding'}
            <div class="bar"><div class="fill" style:width="{job.progress * 100}%"></div></div>
          {/if}
        </div>
        <button
          class="icon-btn"
          title={job.status === 'done' || job.status === 'error' ? 'Dismiss' : 'Cancel download'}
          onclick={() => dismiss(job)}
        >
          <Icon name="close" size={16} />
        </button>
      </div>
    {/each}
  </div>
{/if}

<ul class="list">
  {#each results as video (video.id)}
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
        {#if video.track_id || job?.status === 'done'}<span class="badge queue">In library</span>{/if}
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
  {/each}
</ul>

{#if searched && !searching && !results.length}
  <div class="empty">No videos found for “{searched}”.</div>
{:else if !searched && !downloads.length}
  <div class="empty">
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

  .jobs {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-bottom: 12px;
    padding-bottom: 12px;
    border-bottom: 1px solid var(--border);
  }

  .job {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
  }

  .job.error .sub {
    color: var(--live);
  }

  .thumb.tiny {
    width: 56px;
  }

  .bar {
    height: 4px;
    margin-top: 4px;
    border-radius: 99px;
    background: var(--surface-3);
    overflow: hidden;
  }

  .fill {
    height: 100%;
    background: var(--accent);
    transition: width 0.4s ease;
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
