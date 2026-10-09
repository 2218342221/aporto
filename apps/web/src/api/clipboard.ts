/** Clipboard API on HTTPS, with a user-initiated fallback for local HTTP deployments. */
export async function copyText(text: string): Promise<void> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text);
      return;
    }
  } catch {
    // Browser permissions can deny the async API while a focused copy still works.
  }

  const focused = document.activeElement;
  const input =
    focused instanceof HTMLInputElement || focused instanceof HTMLTextAreaElement ? focused : null;
  const selection = input
    ? { start: input.selectionStart, end: input.selectionEnd, direction: input.selectionDirection }
    : null;
  const pageSelection = window.getSelection();
  const ranges = pageSelection
    ? Array.from({ length: pageSelection.rangeCount }, (_, index) =>
        pageSelection.getRangeAt(index).cloneRange(),
      )
    : [];
  const field = document.createElement('textarea');
  field.value = text;
  field.readOnly = true;
  field.setAttribute('aria-hidden', 'true');
  field.style.cssText = 'position:fixed;top:0;left:0;width:1px;height:1px;opacity:0;';
  document.body.append(field);
  try {
    field.select();
    if (typeof document.execCommand !== 'function' || !document.execCommand('copy'))
      throw new Error('无法复制，请手动选择文本');
  } finally {
    field.remove();
    if (focused instanceof HTMLElement) focused.focus({ preventScroll: true });
    if (input && selection?.start != null && selection.end != null) {
      input.setSelectionRange(selection.start, selection.end, selection.direction ?? undefined);
    } else if (pageSelection) {
      pageSelection.removeAllRanges();
      for (const range of ranges) pageSelection.addRange(range);
    }
  }
}
