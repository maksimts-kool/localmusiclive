<script>
  import Icon from './Icon.svelte';
  import { api } from './api.js';
  import { attempt } from './toast.svelte.js';

  let { downloads } = $props();

  const BUSY = ['waiting', 'downloading', 'adding'];
  let active = $derived(downloads.filter((d) => BUSY.includes(d.status)).length);
  let finished = $derived(downloads.filter((d) => !BUSY.includes(d.status)));

  function line(job) {
    switch (job.status) {
      case 'waiting':
        return 'Waiting…';
      case 'downloading':
        return `${Math.round(job.progress * 100)}%`;
      case 'adding':
        return 'Adding to library…';
      case 'done':
        return { queue: 'Added and queued', next: 'Added, plays next', none: 'Added to library' }[job.then];
      default:
        return job.error || 'Failed';
    }
  }

  function dismiss(job) {
    attempt(() => api.del(`/api/youtube/downloads/${job.id}`));
  }

  function clearFinished() {
    for (const job of finished) dismiss(job);
  }
</script>

<div class="downloads" aria-live="polite">
  <div class="label">
    {#if active}
      <Icon name="loader" size={14} />
      <span>Downloading <b>{active}</b></span>
    {:else}
      <Icon name="download" size={14} />
      <span>Downloads</span>
    {/if}
  </div>

  <div class="jobs">
    {#each downloads as job (job.id)}
      <div class="job {job.status}" title={job.title}>
        <div class="thumb" style:background-image="url({job.thumb})"></div>
        <div class="text">
          <div class="ellipsis title">{job.title}</div>
          <div class="ellipsis sub">
            {#if job.status === 'done'}<Icon name="check" size={12} />{/if}
            {line(job)}
          </div>
          {#if BUSY.includes(job.status)}
            <div class="bar" class:indeterminate={job.status !== 'downloading'}>
              <div class="fill" style:width="{job.status === 'downloading' ? job.progress * 100 : 100}%"></div>
            </div>
          {/if}
        </div>
        <button
          class="icon-btn"
          title={BUSY.includes(job.status) ? 'Cancel download' : 'Dismiss'}
          onclick={() => dismiss(job)}
        >
          <Icon name="close" size={14} />
        </button>
      </div>
    {/each}
  </div>

  {#if finished.length}
    <button class="btn clear" onclick={clearFinished}>Clear finished</button>
  {/if}
</div>

<style>
  .downloads {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 20px;
    border-bottom: 1px solid var(--border);
    background: color-mix(in srgb, var(--surface) 92%, transparent);
    min-width: 0;
  }

  .label {
    display: flex;
    align-items: center;
    gap: 6px;
    flex: none;
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    color: var(--muted);
  }

  .label b {
    color: var(--accent);
  }

  .jobs {
    flex: 1;
    display: flex;
    gap: 8px;
    overflow-x: auto;
    min-width: 0;
    scrollbar-width: thin;
  }

  .job {
    flex: 0 0 260px;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 5px 4px 5px 5px;
    border-radius: 8px;
    background: var(--surface-2);
    border: 1px solid var(--border);
    min-width: 0;
  }

  .thumb {
    width: 48px;
    border-radius: 4px;
  }

  .text {
    flex: 1;
    min-width: 0;
  }

  .title {
    font-size: 12.5px;
    font-weight: 600;
  }

  .sub {
    display: flex;
    align-items: center;
    gap: 3px;
    font-size: 11.5px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }

  .job.done .sub {
    color: var(--ok);
  }

  .job.error {
    border-color: color-mix(in srgb, var(--live) 50%, var(--border));
  }

  .job.error .sub {
    color: var(--live);
  }

  .bar {
    height: 3px;
    margin-top: 3px;
    border-radius: 99px;
    background: var(--surface-3);
    overflow: hidden;
  }

  .fill {
    height: 100%;
    background: var(--accent);
    transition: width 0.4s ease;
  }

  .bar.indeterminate .fill {
    opacity: 0.5;
    animation: pulse 1.2s ease-in-out infinite;
  }

  @keyframes pulse {
    50% {
      opacity: 0.15;
    }
  }

  .icon-btn {
    width: 26px;
    height: 26px;
  }

  .clear {
    flex: none;
    height: 28px;
    font-size: 12.5px;
  }

  @media (max-width: 960px) {
    .downloads {
      padding: 8px 16px;
    }
    .label span {
      display: none;
    }
    .job {
      flex-basis: 220px;
    }
  }
</style>
