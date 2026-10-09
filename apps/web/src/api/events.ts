/** Streaming SSE parser. Handles split UTF-8/CRLF, comments and multiline data. */
import { ProtocolError } from './validation';
export interface SseMessage {
  id?: string;
  event: string;
  data: string;
}
export class SseParser {
  private buffer = '';
  private lines: string[] = [];
  private frameBytes = 0;
  private pendingCR = false;
  private encoder = new TextEncoder();

  constructor(
    private readonly emit: (message: SseMessage) => void,
    private readonly maxFrame = 2 * 1024 * 1024,
  ) {}

  feed(chunk: string) {
    if (!chunk) return;
    // A CR at a chunk boundary already ended a line; consume the optional LF.
    if (this.pendingCR) {
      if (chunk.startsWith('\n')) chunk = chunk.slice(1);
      this.pendingCR = false;
    }
    let start = 0;
    for (let i = 0; i < chunk.length; i++) {
      const char = chunk[i];
      if (char !== '\n' && char !== '\r') continue;
      const fragment = chunk.slice(start, i);
      this.append(fragment);
      this.frameBytes++;
      this.checkSize();
      const line = this.buffer;
      this.buffer = '';
      if (char === '\r') {
        if (chunk[i + 1] === '\n') i++;
        else if (i === chunk.length - 1) this.pendingCR = true;
      }
      start = i + 1;
      this.line(line);
    }
    this.append(chunk.slice(start));
  }

  private append(fragment: string) {
    this.frameBytes += this.encoder.encode(fragment).byteLength;
    this.checkSize();
    this.buffer += fragment;
  }

  private checkSize() {
    if (this.frameBytes > this.maxFrame) throw new ProtocolError('事件大小超过客户端限制');
  }

  private line(line: string) {
    if (line !== '') {
      this.lines.push(line);
      return;
    }
    let id: string | undefined;
    let event = 'message';
    const data: string[] = [];
    for (const entry of this.lines) {
      if (entry.startsWith(':')) continue;
      const colon = entry.indexOf(':');
      const field = colon < 0 ? entry : entry.slice(0, colon);
      let value = colon < 0 ? '' : entry.slice(colon + 1);
      if (value.startsWith(' ')) value = value.slice(1);
      if (field === 'data') data.push(value);
      if (field === 'event') event = value;
      if (field === 'id' && !value.includes('\0')) id = value;
    }
    this.lines = [];
    this.frameBytes = 0;
    if (data.length) this.emit({ id, event, data: data.join('\n') });
  }
}
