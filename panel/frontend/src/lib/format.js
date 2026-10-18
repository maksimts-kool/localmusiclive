export function duration(seconds) {
  if (seconds == null || !Number.isFinite(seconds)) return '–:––';
  const s = Math.max(0, Math.floor(seconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, '0');
  return h ? `${h}:${String(m).padStart(2, '0')}:${sec}` : `${m}:${sec}`;
}

export function eta(seconds) {
  if (seconds == null) return '';
  if (seconds < 60) return 'in <1 min';
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `in ${minutes} min`;
  const h = Math.floor(minutes / 60);
  const m = minutes % 60;
  return m ? `in ${h} h ${m} min` : `in ${h} h`;
}

export function clock(timestamp) {
  return new Date(timestamp * 1000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

export function trackName(track) {
  if (!track) return '';
  return track.artist ? `${track.artist} – ${track.title}` : track.title;
}
