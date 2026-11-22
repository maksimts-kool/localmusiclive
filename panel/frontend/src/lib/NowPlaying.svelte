<script>
  import Icon from './Icon.svelte';
  import LevelPicker from './LevelPicker.svelte';
  import { api } from './api.js';
  import { toast } from './toast.svelte.js';
  import { duration, trackName } from './format.js';
  import { GOOD } from './levels.js';

  let { now, next, receivedAt, running, onSkip } = $props();

  let tick = $state(performance.now() / 1000);
  $effect(() => {
    const id = setInterval(() => (tick = performance.now() / 1000), 250);
    return () => clearInterval(id);
  });

  let track = $derived(now?.track);
  let position = $derived(
    now && now.length ? Math.min(Math.max(now.position + (tick - receivedAt), 0), now.length) : 0,
  );
  let percent = $derived(now?.length ? (position / now.length) * 100 : 0);
  let skipping = $state(false);

  // Shown right away; the next state update confirms it.
  let picked = $state(null);
  let level = $derived(picked?.id === track?.id ? picked.level : (track?.level ?? GOOD));

  async function rate(value) {
    const id = track.id;
    picked = { id, level: value };
    try {
      await api.put(`/api/tracks/${id}/level`, { level: value });
    } catch (error) {
      toast(error.message, 'error');
    }
    // The state update for the change went out before this answer.
    if (picked?.id === id && picked.level === value) picked = null;
  }

  async function skip() {
    skipping = true;
    await onSkip();
    setTimeout(() => (skipping = false), 1200);
  }
</script>

<section class="card now">
  <div class="art thumb" style:background-image={track?.thumb ? `url(${track.thumb})` : null}>
    {#if !track?.thumb}<Icon name="music" size={40} />{/if}
  </div>

  <div class="info">
    <div class="eyebrow">
      <h2 class="section-title">Now playing</h2>
      {#if now?.kind === 'queue'}<span class="badge queue">Queued</span>{/if}
      {#if now?.kind === 'auto'}<span class="badge">Autoplay</span>{/if}
    </div>

    {#if track}
      <div class="title ellipsis" title={track.title}>{track.title}</div>
      <div class="artist ellipsis">{track.artist || track.path}</div>
      {#if track.id != null}
        <div class="rate"><LevelPicker small value={level} onPick={rate} /></div>
      {/if}
    {:else if now?.kind === 'placeholder'}
      <div class="title">Placeholder on air</div>
      <div class="artist">ffplayout is sending black and silence. Queue something or turn on autoplay.</div>
    {:else if running}
      <div class="title muted">Waiting for ffplayout…</div>
    {:else}
      <div class="title muted">Playout is stopped</div>
      <div class="artist">Press Start to go on air.</div>
    {/if}

    <div class="progress" aria-hidden={!track}>
      <div class="bar"><div class="fill" style:width="{percent}%"></div></div>
      <div class="times">
        <span>{duration(position)}</span>
        <span>{track ? `−${duration(now.length - position)}` : ''}</span>
      </div>
    </div>

    <div class="actions">
      <button class="btn primary" onclick={skip} disabled={!running || !now || skipping}>
        <Icon name="skip" size={16} /> Skip
      </button>
      {#if next?.track}
        <div class="next ellipsis">
          <span class="muted">Next</span>
          {trackName(next.track)}
        </div>
      {/if}
    </div>
  </div>
</section>

<style>
  .now {
    display: flex;
    gap: 18px;
    padding: 16px;
    align-items: stretch;
  }

  .art {
    width: 240px;
    align-self: flex-start;
    border-radius: 8px;
  }

  .info {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .eyebrow {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 4px;
  }

  .title {
    font-size: 22px;
    font-weight: 650;
    line-height: 1.25;
  }

  .artist {
    color: var(--muted);
    font-size: 15px;
  }

  .rate {
    margin-top: 8px;
  }

  .progress {
    margin-top: auto;
    padding-top: 14px;
  }

  .bar {
    height: 6px;
    border-radius: 99px;
    background: var(--surface-3);
    overflow: hidden;
  }

  .fill {
    height: 100%;
    background: var(--accent);
    transition: width 0.25s linear;
  }

  .times {
    display: flex;
    justify-content: space-between;
    font-size: 12px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
    margin-top: 4px;
  }

  .actions {
    display: flex;
    align-items: center;
    gap: 14px;
    margin-top: 10px;
    min-width: 0;
  }

  .next {
    min-width: 0;
    font-size: 13px;
  }

  .next .muted {
    margin-right: 6px;
    font-weight: 600;
  }

  @media (max-width: 640px) {
    .now {
      flex-direction: column;
    }
    .art {
      width: 100%;
    }
  }
</style>
