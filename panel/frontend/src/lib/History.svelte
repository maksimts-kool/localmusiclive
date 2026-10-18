<script>
  import { clock, duration } from './format.js';

  let { history } = $props();
</script>

<h2 class="section-title head">Recently played</h2>

{#if history.length}
  <ol class="list">
    {#each history as h (h.id)}
      <li class="row">
        <span class="time">{clock(h.started_at)}</span>
        <div class="thumb small" style:background-image={h.track?.thumb ? `url(${h.track.thumb})` : null}></div>
        <div class="text">
          <div class="ellipsis title">{h.track?.title ?? 'Removed track'}</div>
          <div class="ellipsis muted sub">{h.track?.artist}</div>
        </div>
        {#if h.skipped}<span class="badge skipped">Skipped</span>{/if}
        <span class="badge" class:queue={h.kind === 'queue'}>{h.kind === 'queue' ? 'Queued' : 'Autoplay'}</span>
        <span class="muted dur">{h.ended_at ? duration(h.ended_at - h.started_at) : ''}</span>
      </li>
    {/each}
  </ol>
{:else}
  <div class="empty">Nothing played yet.</div>
{/if}

<style>
  .head {
    display: flex;
    align-items: center;
    min-height: 34px;
    margin-bottom: 6px;
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
    min-width: 0;
  }

  .time {
    width: 44px;
    flex: none;
    font-size: 12px;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
  }

  .thumb.small {
    width: 56px;
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
    width: 40px;
    text-align: right;
    font-size: 12px;
    font-variant-numeric: tabular-nums;
    flex: none;
  }
</style>
