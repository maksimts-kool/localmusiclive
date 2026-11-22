// Song levels, best first. Every new song starts out Хорошо.
export const LEVELS = [
  { value: 2, name: 'Отлично', key: 'great' },
  { value: 1, name: 'Хорошо', key: 'good' },
  { value: 0, name: 'Плохо', key: 'bad' },
];

export const GOOD = 1;

export function levelOf(value) {
  return LEVELS.find((l) => l.value === value) ?? LEVELS[1];
}
