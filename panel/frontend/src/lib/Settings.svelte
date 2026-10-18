<script>
  import { untrack } from 'svelte';
  import Icon from './Icon.svelte';
  import { api } from './api.js';
  import { attempt } from './toast.svelte.js';

  let { autoplay, overlay, resolution = [1280, 720], onClose } = $props();

  // Edit copies of the settings at the time the dialog opened; saved explicitly.
  let ap = $state(untrack(() => ({ ...autoplay })));
  let ov = $state(untrack(() => ({ ...overlay })));
  let folders = $state([]);
  let dialog;

  $effect(() => {
    dialog.showModal();
    api.get('/api/library/folders').then((f) => (folders = f)).catch(() => {});
  });

  const positions = [
    ['top-left', 'Top left'],
    ['top-center', 'Top centre'],
    ['top-right', 'Top right'],
    ['bottom-left', 'Bottom left'],
    ['bottom-center', 'Bottom centre'],
    ['bottom-right', 'Bottom right'],
  ];

  const numeric = [
    'margin', 'scale', 'start_seconds', 'end_seconds', 'radius',
    'stroke_width', 'stroke_opacity', 'background_opacity', 'line_height',
  ];

  function numbers(values) {
    const out = { ...values };
    for (const key of numeric) out[key] = Number(out[key]);
    return out;
  }

  // Which card the preview shows; on the stream they alternate by themselves.
  // "line" is the progress line shown between the cards.
  let card = $state('now');
  let lineShown = $derived(ov.progress_line && !(ov.enabled && ov.mode === 'always'));
  let shown = $derived.by(() => {
    if (!ov.enabled) return lineShown ? 'line' : 'now';
    if (ov.mode === 'always') return 'now';
    if ((card === 'line' && !lineShown) || (card === 'next' && !ov.show_next)) return 'now';
    return card;
  });
  let previewUrl = $state(null);

  // The preview is the real card, drawn by the panel with the unsaved settings.
  $effect(() => {
    const body = numbers({ ...ov, card: shown });
    const controller = new AbortController();
    const timer = setTimeout(async () => {
      try {
        const res = await fetch('/api/overlay/preview', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify(body),
          signal: controller.signal,
        });
        if (!res.ok) return;
        const url = URL.createObjectURL(await res.blob());
        if (previewUrl) URL.revokeObjectURL(previewUrl);
        previewUrl = url;
      } catch {
        // superseded by a newer preview
      }
    }, 120);
    return () => {
      clearTimeout(timer);
      controller.abort();
    };
  });

  async function saveAutoplay() {
    const ok = await attempt(
      () => api.put('/api/settings/autoplay', { ...ap, avoid_repeat: Number(ap.avoid_repeat) }),
      'Autoplay settings saved',
    );
    if (ok) onClose();
  }

  async function saveOverlay() {
    const ok = await attempt(() => api.put('/api/settings/overlay', numbers(ov)), 'Song card updated');
    if (ok) onClose();
  }
</script>

<dialog bind:this={dialog} onclose={onClose} class="card">
  <header>
    <h2>Settings</h2>
    <button class="icon-btn" title="Close" onclick={() => dialog.close()}><Icon name="close" /></button>
  </header>

  <section>
    <h3 class="section-title">Autoplay</h3>
    <p class="muted hint">Plays from the library whenever the queue is empty.</p>
    <div class="grid">
      <label class="check"><input type="checkbox" bind:checked={ap.enabled} /> Autoplay when the queue is empty</label>
      <label>
        <span>Order</span>
        <select bind:value={ap.mode} disabled={!ap.enabled}>
          <option value="shuffle">Shuffle</option>
          <option value="least_played">Least played first</option>
          <option value="in_order">In library order</option>
        </select>
      </label>
      <label>
        <span>Only from folder</span>
        <select bind:value={ap.folder} disabled={!ap.enabled}>
          <option value="">Whole library</option>
          {#each folders as f}<option value={f}>{f}</option>{/each}
        </select>
      </label>
      <label>
        <span>Don't repeat within (songs)</span>
        <input type="number" min="0" max="500" bind:value={ap.avoid_repeat} disabled={!ap.enabled || ap.mode === 'in_order'} />
      </label>
    </div>
    <div class="row-end"><button class="btn primary" onclick={saveAutoplay}>Save autoplay</button></div>
  </section>

  <section>
    <h3 class="section-title">Song card on the stream</h3>
    <p class="muted hint">
      Shows the song as it starts and the next one before it ends, with a live countdown. In
      between, a thin progress line runs along the bottom edge.
    </p>

    {#if !(ov.enabled && ov.mode === 'always')}
      <div class="segmented" role="radiogroup" aria-label="Preview">
        <button
          role="radio"
          aria-checked={shown === 'now'}
          class:on={shown === 'now'}
          disabled={!ov.enabled}
          onclick={() => (card = 'now')}
        >
          Song start
        </button>
        <button
          role="radio"
          aria-checked={shown === 'line'}
          class:on={shown === 'line'}
          disabled={!lineShown}
          onclick={() => (card = 'line')}
        >
          Mid-song
        </button>
        <button
          role="radio"
          aria-checked={shown === 'next'}
          class:on={shown === 'next'}
          disabled={!ov.enabled || !ov.show_next}
          onclick={() => (card = 'next')}
        >
          Song end
        </button>
      </div>
    {/if}
    <div class="preview" style:aspect-ratio="{resolution[0]} / {resolution[1]}">
      {#if (ov.enabled || shown === 'line') && previewUrl}<img src={previewUrl} alt="Preview of the song card" />{/if}
    </div>

    <div class="group">
      <label class="check"><input type="checkbox" bind:checked={ov.enabled} /> Show the song card</label>
      <label class="check">
        <input type="checkbox" bind:checked={ov.show_next} disabled={!ov.enabled} /> Announce the next song
      </label>
      <label class="check">
        <input type="checkbox" bind:checked={ov.progress_line} /> Progress line between the cards
      </label>
    </div>
    <div class="grid">
      <label>
        <span>When</span>
        <select bind:value={ov.mode} disabled={!ov.enabled}>
          <option value="start_end">Start and end of each song</option>
          <option value="always">All the time</option>
        </select>
      </label>
      <label>
        <span>“Now playing” for the first (s)</span>
        <input type="number" min="0" max="600" bind:value={ov.start_seconds} disabled={!ov.enabled || ov.mode !== 'start_end'} />
      </label>
      <label>
        <span>“Up next” for the last (s)</span>
        <input
          type="number"
          min="0"
          max="600"
          bind:value={ov.end_seconds}
          disabled={!ov.enabled || ov.mode !== 'start_end' || !ov.show_next}
        />
      </label>
      <label>
        <span>Position</span>
        <select bind:value={ov.position}>
          {#each positions as [value, name]}<option {value}>{name}</option>{/each}
        </select>
      </label>
      <label>
        <span>Distance from edge (px)</span>
        <input type="number" min="0" max="400" bind:value={ov.margin} />
      </label>
      <label>
        <span>Size ({ov.scale} %)</span>
        <input type="range" min="50" max="250" step="5" bind:value={ov.scale} />
      </label>
    </div>

    <h4 class="sub-title">Look</h4>
    <div class="grid">
      <label class="color">
        <span>Accent</span>
        <input type="color" bind:value={ov.accent_color} />
      </label>
      <label class="color">
        <span>Background</span>
        <input type="color" bind:value={ov.background_color} />
      </label>
      <label>
        <span>Background opacity</span>
        <input type="range" min="0" max="1" step="0.02" bind:value={ov.background_opacity} />
      </label>
      <label>
        <span>Corner radius ({ov.radius} px)</span>
        <input type="range" min="0" max="40" step="1" bind:value={ov.radius} />
      </label>
      <label class="check"><input type="checkbox" bind:checked={ov.stroke} /> Border</label>
      <label class="color">
        <span>Border colour</span>
        <input type="color" bind:value={ov.stroke_color} disabled={!ov.stroke} />
      </label>
      <label>
        <span>Border opacity</span>
        <input type="range" min="0" max="1" step="0.02" bind:value={ov.stroke_opacity} disabled={!ov.stroke} />
      </label>
      <label>
        <span>Border width ({ov.stroke_width} px)</span>
        <input type="range" min="0.5" max="6" step="0.5" bind:value={ov.stroke_width} disabled={!ov.stroke} />
      </label>
      <label class="check"><input type="checkbox" bind:checked={ov.shadow} /> Shadow</label>
      <label class="check"><input type="checkbox" bind:checked={ov.show_thumbnail} /> Thumbnail</label>
      <label class="check"><input type="checkbox" bind:checked={ov.show_progress} /> Progress bar</label>
      <label>
        <span>Progress line thickness ({ov.line_height} px)</span>
        <input type="range" min="1" max="12" step="1" bind:value={ov.line_height} disabled={!ov.progress_line} />
      </label>
    </div>
    <div class="row-end"><button class="btn primary" onclick={saveOverlay}>Apply on stream</button></div>
  </section>
</dialog>

<style>
  dialog {
    width: min(720px, calc(100vw - 32px));
    max-height: calc(100vh - 48px);
    padding: 0;
    color: var(--text);
    box-shadow: var(--shadow);
    overflow: auto;
  }

  dialog::backdrop {
    background: rgb(0 0 0 / 0.5);
  }

  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 14px 16px 6px;
    position: sticky;
    top: 0;
    background: var(--surface);
    z-index: 1;
  }

  h2 {
    margin: 0;
    font-size: 17px;
  }

  section {
    padding: 12px 16px 16px;
    border-top: 1px solid var(--border);
  }

  section:first-of-type {
    border-top: 0;
  }

  .hint {
    margin: 4px 0 10px;
    font-size: 13px;
  }

  .sub-title {
    margin: 18px 0 0;
    font-size: 13px;
    color: var(--muted);
  }

  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
    gap: 12px 16px;
    margin-top: 10px;
  }

  .group {
    display: flex;
    flex-wrap: wrap;
    gap: 4px 20px;
    margin-top: 10px;
  }

  label {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: 13px;
  }

  label > span {
    color: var(--muted);
  }

  label.check {
    flex-direction: row;
    align-items: center;
    gap: 8px;
    align-self: end;
    min-height: 34px;
  }

  label.color input {
    width: 64px;
    height: 34px;
    padding: 2px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--surface-2);
  }

  input[type='range'] {
    height: 34px;
  }

  .row-end {
    display: flex;
    justify-content: flex-end;
    margin-top: 14px;
  }

  .preview {
    position: relative;
    width: 100%;
    max-width: 560px;
    margin-top: 10px;
    border-radius: 8px;
    overflow: hidden;
    background: linear-gradient(135deg, #2b3a67 0%, #6b3f74 50%, #c06c4c 100%);
  }

  .preview img {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
  }

  .segmented {
    display: inline-flex;
    margin-top: 4px;
    padding: 3px;
    border-radius: 9px;
    background: var(--surface-3);
  }

  .segmented button {
    border: 0;
    border-radius: 7px;
    padding: 4px 12px;
    background: transparent;
    color: var(--muted);
    font-size: 13px;
    font-weight: 600;
  }

  .segmented button.on {
    background: var(--surface);
    color: var(--text);
    box-shadow: 0 1px 3px rgb(0 0 0 / 0.2);
  }

  .segmented button:disabled {
    opacity: 0.4;
  }
</style>
