<script>
  import { untrack } from 'svelte';
  import Icon from './Icon.svelte';
  import { api } from './api.js';
  import { attempt } from './toast.svelte.js';

  let { autoplay, overlay, now, next, resolution = [1280, 720], onClose } = $props();

  // Edit copies of the settings at the time the dialog opened; saved explicitly.
  let ap = $state(untrack(() => ({ ...autoplay })));
  let ov = $state(untrack(() => ({ ...overlay })));
  let fonts = $state(untrack(() => [overlay.font_family]));
  let folders = $state([]);
  let dialog;

  $effect(() => {
    dialog.showModal();
    api.get('/api/fonts').then((f) => (fonts = f)).catch(() => {});
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

  function label(track) {
    return track ? (track.artist ? `${track.artist} – ${track.title}` : track.title) : null;
  }

  let previewText = $derived(
    [
      `Now: ${label(now?.track) ?? 'Artist – Song title'}`,
      ov.show_next ? `Next: ${label(next?.track) ?? 'Another artist – Next song'}` : null,
    ]
      .filter(Boolean)
      .join('  ·  '),
  );

  // The preview is drawn at the stream's resolution and scaled down to fit.
  let previewStyle = $derived.by(() => {
    const [v, h] = ov.position.split('-');
    const m = `${ov.margin}px`;
    const s = [
      `font-family: '${ov.font_family}', system-ui, sans-serif`,
      `font-size: ${ov.font_size}px`,
      `color: ${ov.text_color}`,
      `padding: ${ov.background ? ov.padding : 0}px`,
      `background: ${ov.background ? hexAlpha(ov.background_color, ov.background_opacity) : 'transparent'}`,
      v === 'top' ? `top: ${m}` : `bottom: ${m}`,
    ];
    if (h === 'left') s.push(`left: ${m}`);
    else if (h === 'right') s.push(`right: ${m}`);
    else s.push('left: 50%', 'transform: translateX(-50%)');
    return s.join(';');
  });

  function hexAlpha(hex, alpha) {
    const n = parseInt(hex.slice(1), 16);
    return `rgb(${n >> 16} ${(n >> 8) & 255} ${n & 255} / ${alpha})`;
  }

  let previewWidth = $state(560);

  async function saveAutoplay() {
    const ok = await attempt(
      () => api.put('/api/settings/autoplay', { ...ap, avoid_repeat: Number(ap.avoid_repeat) }),
      'Autoplay settings saved',
    );
    if (ok) onClose();
  }

  async function saveOverlay() {
    const ok = await attempt(
      () =>
        api.put('/api/settings/overlay', {
          ...ov,
          margin: Number(ov.margin),
          font_size: Number(ov.font_size),
          padding: Number(ov.padding),
          background_opacity: Number(ov.background_opacity),
        }),
      'On-screen text updated',
    );
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
    <h3 class="section-title">On-screen text</h3>
    <div class="preview" bind:clientWidth={previewWidth} style:aspect-ratio="{resolution[0]} / {resolution[1]}">
      <div
        class="canvas"
        style:width="{resolution[0]}px"
        style:height="{resolution[1]}px"
        style:transform="scale({previewWidth / resolution[0]})"
      >
        {#if ov.enabled}<div class="text" style={previewStyle}>{previewText}</div>{/if}
      </div>
    </div>
    <div class="grid">
      <label class="check"><input type="checkbox" bind:checked={ov.enabled} /> Show “Now / Next” text</label>
      <label class="check"><input type="checkbox" bind:checked={ov.show_next} disabled={!ov.enabled} /> Include the next song</label>
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
        <span>Font</span>
        <select bind:value={ov.font_family}>
          {#each fonts as f}<option value={f}>{f}</option>{/each}
        </select>
      </label>
      <label>
        <span>Size (px)</span>
        <input type="number" min="10" max="120" bind:value={ov.font_size} />
      </label>
      <label class="color">
        <span>Text colour</span>
        <input type="color" bind:value={ov.text_color} />
      </label>
      <label class="check"><input type="checkbox" bind:checked={ov.background} /> Background box</label>
      <label class="color">
        <span>Box colour</span>
        <input type="color" bind:value={ov.background_color} disabled={!ov.background} />
      </label>
      <label>
        <span>Box opacity</span>
        <input type="range" min="0" max="1" step="0.05" bind:value={ov.background_opacity} disabled={!ov.background} />
      </label>
      <label>
        <span>Box padding (px)</span>
        <input type="number" min="0" max="80" bind:value={ov.padding} disabled={!ov.background} />
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

  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
    gap: 12px 16px;
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
    max-width: 520px;
    margin-top: 10px;
    border-radius: 8px;
    overflow: hidden;
    background:
      linear-gradient(135deg, #2b3a67 0%, #6b3f74 50%, #c06c4c 100%);
  }

  .canvas {
    position: absolute;
    top: 0;
    left: 0;
    transform-origin: 0 0;
  }

  .text {
    position: absolute;
    white-space: nowrap;
    line-height: 1.2;
  }
</style>
