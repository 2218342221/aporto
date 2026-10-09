import { afterEach, describe, expect, it, vi } from 'vitest';
import { createIdempotencyKey } from './idempotency';

afterEach(() => vi.unstubAllGlobals());

describe('idempotency keys across browser security contexts', () => {
  it('prefers the native UUID generator when available', () => {
    const expected = '123e4567-e89b-42d3-a456-426614174000';
    const randomUUID = vi.fn(() => expected);
    const getRandomValues = vi.fn();
    vi.stubGlobal('crypto', { randomUUID, getRandomValues });

    expect(createIdempotencyKey()).toBe(expected);
    expect(randomUUID).toHaveBeenCalledTimes(1);
    expect(getRandomValues).not.toHaveBeenCalled();
  });

  it('uses all 16 random bytes when randomUUID is unavailable on an HTTP origin', () => {
    const getRandomValues = vi.fn((bytes: Uint8Array) => {
      bytes.set(Array.from({ length: 16 }, (_, index) => index));
      return bytes;
    });
    vi.stubGlobal('crypto', { getRandomValues });

    expect(createIdempotencyKey()).toBe('00010203-0405-4607-8809-0a0b0c0d0e0f');
    expect(getRandomValues).toHaveBeenCalledTimes(1);
    expect(getRandomValues.mock.calls[0][0]).toHaveLength(16);
  });

  it.each([0x00, 0xff])('sets UUID v4 version and RFC variant for random byte %i', (value) => {
    vi.stubGlobal('crypto', {
      randomUUID: undefined,
      getRandomValues: (bytes: Uint8Array) => bytes.fill(value),
    });

    expect(createIdempotencyKey()).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
    );
  });

  it('fails explicitly if no cryptographic random generator is available', () => {
    vi.stubGlobal('crypto', undefined);
    expect(() => createIdempotencyKey()).toThrow('无法生成安全的请求标识');
  });
});
