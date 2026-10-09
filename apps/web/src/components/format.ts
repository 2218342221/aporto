export function timeLabel(timestamp: number) {
  return new Intl.DateTimeFormat('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  }).format(timestamp);
}
export function fullTime(timestamp: number) {
  return new Intl.DateTimeFormat('zh-CN', {
    dateStyle: 'medium',
    timeStyle: 'medium',
    hour12: false,
  }).format(timestamp);
}
export function shortDate(timestamp: number) {
  const today = new Date();
  const date = new Date(timestamp);
  return today.toDateString() === date.toDateString()
    ? new Intl.DateTimeFormat('zh-CN', {
        hour: '2-digit',
        minute: '2-digit',
        hour12: false,
      }).format(date)
    : new Intl.DateTimeFormat('zh-CN', { month: '2-digit', day: '2-digit' }).format(date);
}
