<script>
  import Icon from './lib/Icon.svelte';
  import NowPlaying from './lib/NowPlaying.svelte';
  import Queue from './lib/Queue.svelte';
  import Library from './lib/Library.svelte';
  import History from './lib/History.svelte';
  import Settings from './lib/Settings.svelte';
  import YouTube from './lib/YouTube.svelte';
  import Downloads from './lib/Downloads.svelte';
  import { api } from './lib/api.js';
  import { attempt, toasts } from './lib/toast.svelte.js';

  let data = $state(null);
  let receivedAt = $state(0);
  let connected = $state(false);
  let libraryVersion = $state(0);
  // The latest one-song level change, from any browser.
  let levelChange = $state(null);
  let tab = $state('queue');
  let source = $state('library');
  let settingsOpen = $state(false);
  let headerHeight = $state(56);

  $effect(() => {
    let socket;
    let retry;
    let closed = false;
    function connect() {
      const proto = location.protocol === 'https:' ? 'wss' : 'ws';
      socket = new WebSocket(`${proto}://${location.host}/ws`);
      socket.onopen = () => (connected = true);
      socket.onmessage = (event) => {
        const message = JSON.parse(event.data);
        if (message.type === 'state') {
          data = message.data;
          receivedAt = performance.now() / 1000;
        } else if (message.type === 'library') {
          libraryVersion++;
        } else if (message.type === 'level') {
          levelChange = { id: message.id, level: message.level };
        }
      };
      socket.onclose = () => {
        connected = false;
        if (!closed) retry = setTimeout(connect, 1500);
      };
    }
    connect();
    return () => {
      closed = true;
      clearTimeout(retry);
      socket?.close();
    };
  });

  let status = $derived(data?.status);
  let running = $derived(Boolean(status?.running));
  let downloads = $derived(data?.downloads ?? []);
  let downloading = $derived(downloads.filter((d) => ['waiting', 'downloading', 'adding'].includes(d.status)).length);
  let queuedIds = $derived(new Set((data?.queue ?? []).map((q) => q.track?.id)));

  let pill = $derived.by(() => {
    if (!connected) return { text: 'Panel offline', tone: 'idle' };
    if (!status) return { text: 'Connecting…', tone: 'idle' };
    if (status.ffplayout !== 'ok') return { text: `ffplayout ${status.ffplayout}`, tone: 'warn' };
    if (!running) return { text: 'Stopped', tone: 'idle' };
    return ['stream', 'hls'].includes(status.mode) ? { text: 'On air', tone: 'live' } : { text: status.mode, tone: 'preview' };
  });

  let output = $derived.by(() => {
    if (!status?.mode) return '';
    if (status.mode === 'stream') return status.output ? `RTMP · ${status.output}` : 'RTMP stream';
    if (status.mode === 'hls') return status.watch_port ? `On your network · port ${status.watch_port}` : 'HLS';
    return status.mode;
  });

  // The watch page as seen from this browser; on the TV use this Mac's network address.
  let watchUrl = $derived(
    status?.mode === 'hls' && status.watch_port ? `http://${location.hostname}:${status.watch_port}/` : null,
  );

  function playout(command) {
    if (command === 'stop' && !confirm('Stop the stream? Viewers will see it end.')) return;
    attempt(() => api.post(`/api/playout/${command}`));
  }

  function skip() {
    return attempt(() => api.post('/api/skip'));
  }
</script>

<div class="sticky" bind:clientHeight={headerHeight}>
<header class="top">
  <div class="brand">
    <span class="logo"></span>
    <span>localmusiclive</span>
  </div>

  <div class="status">
    <span class="pill {pill.tone}"><span class="dot"></span>{pill.text}</span>
    {#if watchUrl}
      <a class="output ellipsis muted" href={watchUrl} target="_blank" rel="noreferrer" title="Open the watch page">{output}</a>
    {:else if output}<span class="output ellipsis muted">{output}</span>{/if}
  </div>

  <div class="controls">
    {#if running}
      <button class="btn" title="Restart playout" onclick={() => playout('restart')}><Icon name="restart" size={16} /></button>
      <button class="btn danger" onclick={() => playout('stop')}><Icon name="stop" size={14} /> Stop</button>
    {:else}
      <button class="btn primary" onclick={() => playout('start')} disabled={status?.ffplayout !== 'ok'}>
        <Icon name="play" size={14} /> Start
      </button>
    {/if}
    <button class="icon-btn" title="Settings" onclick={() => (settingsOpen = true)} disabled={!data}>
      <Icon name="settings" />
    </button>
  </div>
</header>
{#if downloads.length}<Downloads {downloads} />{/if}
</div>

{#if status?.error && status.ffplayout !== 'ok'}
  <div class="banner warn">Can't reach ffplayout: {status.error}</div>
{/if}
{#each status?.warnings ?? [] as warning}
  <div class="banner warn">{warning}</div>
{/each}

<main style:--header-height="{headerHeight}px">
  <div class="left">
    <NowPlaying now={data?.now} next={data?.next} {receivedAt} {running} onSkip={skip} />

    <section class="card panel">
      <div class="tabs" role="tablist">
        <button role="tab" aria-selected={tab === 'queue'} class:active={tab === 'queue'} onclick={() => (tab = 'queue')}>
          Up next
        </button>
        <button role="tab" aria-selected={tab === 'history'} class:active={tab === 'history'} onclick={() => (tab = 'history')}>
          History
        </button>
      </div>
      {#if tab === 'queue'}
        <Queue queue={data?.queue ?? []} upcoming={data?.upcoming ?? []} autoplay={data?.autoplay} />
      {:else}
        <History history={data?.history ?? []} />
      {/if}
    </section>
  </div>

  <section class="card panel right">
    <div class="tabs" role="tablist">
      <button role="tab" aria-selected={source === 'library'} class:active={source === 'library'} onclick={() => (source = 'library')}>
        Library
      </button>
      <button role="tab" aria-selected={source === 'youtube'} class:active={source === 'youtube'} onclick={() => (source = 'youtube')}>
        YouTube
        {#if downloading}<span class="tab-count">{downloading}</span>{/if}
      </button>
    </div>
    {#if source === 'library'}
      <Library version={libraryVersion} {levelChange} {queuedIds} scanning={status?.scanning} size={status?.library_size ?? 0} />
    {:else}
      <YouTube {downloads} />
    {/if}
  </section>
</main>

{#if settingsOpen && data}
  <Settings
    autoplay={data.autoplay}
    overlay={data.overlay}
    ads={data.ads}
    resolution={status?.resolution}
    onClose={() => (settingsOpen = false)}
  />
{/if}

<div class="toasts" aria-live="polite">
  {#each toasts as t (t.id)}
    <div class="toast {t.kind}">{t.message}</div>
  {/each}
</div>

<style>
  .sticky {
    position: sticky;
    top: 0;
    z-index: 5;
    background: color-mix(in srgb, var(--bg) 88%, transparent);
    backdrop-filter: blur(8px);
  }

  .top {
    display: flex;
    align-items: center;
    gap: 16px;
    padding: 10px 20px;
    border-bottom: 1px solid var(--border);
  }

  .brand {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 700;
    letter-spacing: -0.01em;
    font-size: 15px;
  }

  .logo {
    width: 18px;
    height: 18px;
    border-radius: 5px;
    background: radial-gradient(circle, #fff 0 28%, var(--live) 30%);
  }

  .status {
    flex: 1;
    display: flex;
    align-items: center;
    gap: 12px;
    min-width: 0;
  }

  .output {
    font-size: 12.5px;
  }

  .pill {
    display: inline-flex;
    align-items: center;
    gap: 7px;
    height: 26px;
    padding: 0 11px;
    border-radius: 99px;
    font-size: 12px;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    background: var(--surface-3);
    color: var(--muted);
    flex: none;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: currentColor;
  }

  .pill.live {
    background: var(--live);
    color: #fff;
  }

  .pill.live .dot {
    animation: pulse 1.6s ease-in-out infinite;
  }

  .pill.preview {
    background: color-mix(in srgb, var(--accent) 20%, transparent);
    color: var(--accent);
  }

  .pill.warn {
    background: color-mix(in srgb, var(--warn) 20%, transparent);
    color: var(--warn);
  }

  @keyframes pulse {
    50% {
      opacity: 0.3;
    }
  }

  .controls {
    display: flex;
    align-items: center;
    gap: 8px;
  }

  .banner {
    margin: 12px 20px 0;
    padding: 10px 14px;
    border-radius: 8px;
    font-size: 13px;
  }

  .banner.warn {
    background: color-mix(in srgb, var(--warn) 14%, transparent);
    color: var(--warn);
  }

  main {
    display: grid;
    grid-template-columns: minmax(0, 1.1fr) minmax(0, 1fr);
    gap: 16px;
    padding: 16px 20px 40px;
    max-width: 1500px;
    margin: 0 auto;
    align-items: start;
  }

  .left {
    display: flex;
    flex-direction: column;
    gap: 16px;
    min-width: 0;
  }

  .panel {
    padding: 12px 14px 14px;
    min-width: 0;
  }

  .right {
    position: sticky;
    top: calc(var(--header-height) + 12px);
    max-height: calc(100vh - var(--header-height) - 28px);
    overflow: auto;
  }

  .tabs {
    display: flex;
    gap: 4px;
    margin: -2px 0 10px;
    border-bottom: 1px solid var(--border);
  }

  .tabs button {
    border: 0;
    background: none;
    padding: 8px 10px;
    color: var(--muted);
    font-weight: 600;
    border-bottom: 2px solid transparent;
    margin-bottom: -1px;
  }

  .tab-count {
    display: inline-grid;
    place-items: center;
    min-width: 18px;
    height: 18px;
    margin-left: 4px;
    padding: 0 5px;
    border-radius: 99px;
    background: var(--accent);
    color: var(--accent-text);
    font-size: 11px;
  }

  .tabs button.active {
    color: var(--text);
    border-bottom-color: var(--accent);
  }

  .toasts {
    position: fixed;
    bottom: 16px;
    left: 50%;
    transform: translateX(-50%);
    display: flex;
    flex-direction: column;
    gap: 8px;
    z-index: 20;
    width: max-content;
    max-width: calc(100vw - 32px);
  }

  .toast {
    padding: 9px 14px;
    border-radius: 8px;
    background: var(--surface-3);
    border: 1px solid var(--border);
    box-shadow: var(--shadow);
    font-size: 13px;
  }

  .toast.error {
    background: color-mix(in srgb, var(--live) 22%, var(--surface));
    border-color: var(--live);
  }

  @media (max-width: 960px) {
    main {
      grid-template-columns: 1fr;
      padding: 12px 16px 32px;
    }
    .right {
      position: static;
      max-height: none;
    }
    .top {
      padding: 10px 16px;
      flex-wrap: wrap;
    }
    .output {
      display: none;
    }
  }
</style>
