<script>
  import { LEVELS } from './levels.js';

  // Three buttons, one per level; `value` is lit (null for none).
  let { value = null, onPick, disabled = false, small = false } = $props();
</script>

<div class="levels" class:small role="radiogroup" aria-label="Level">
  {#each LEVELS as l (l.value)}
    <button
      type="button"
      role="radio"
      class="level {l.key}"
      class:on={value === l.value}
      aria-checked={value === l.value}
      {disabled}
      onclick={() => onPick(l.value)}
    >
      {l.name}
    </button>
  {/each}
</div>

<style>
  .levels {
    display: inline-flex;
    gap: 2px;
    padding: 2px;
    border-radius: 99px;
    background: var(--surface-3);
  }

  .level {
    height: 26px;
    padding: 0 12px;
    border: 0;
    border-radius: 99px;
    background: transparent;
    color: var(--muted);
    font-size: 12.5px;
    font-weight: 600;
  }

  .small .level {
    height: 22px;
    padding: 0 10px;
    font-size: 12px;
  }

  .level:hover:not(:disabled) {
    color: var(--text);
  }

  .level.on {
    color: #fff;
  }

  .level.great.on {
    background: var(--ok);
  }

  .level.good.on {
    background: var(--accent);
  }

  .level.bad.on {
    background: var(--live);
  }

  .level:disabled {
    opacity: 0.45;
  }
</style>
