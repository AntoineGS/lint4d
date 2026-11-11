import { describe, expect, it } from 'vitest';
import {
  dcuCapability,
  dcuErrorText,
  STALE_VIEW_TEXT,
} from '../../src/dcuText';

describe('dcuCapability', () => {
  it.each([undefined, {}, { compiledDcuVirtualDocuments: { uriScheme: 5 } }])(
    'returns undefined for unsupported or invalid capabilities',
    (experimental) => {
      expect(dcuCapability(experimental)).toBeUndefined();
    },
  );

  it('returns a valid compiled-document capability', () => {
    expect(dcuCapability({
      compiledDcuVirtualDocuments: {
        uriScheme: 'lint4d-dcu',
        contentMethod: 'textDocument/content',
      },
    })).toEqual({ uriScheme: 'lint4d-dcu', contentMethod: 'textDocument/content' });
  });

  it('defaults the content method when omitted', () => {
    expect(dcuCapability({
      compiledDcuVirtualDocuments: { uriScheme: 'lint4d-dcu' },
    })).toEqual({ uriScheme: 'lint4d-dcu', contentMethod: 'textDocument/content' });
  });
});

describe('dcuErrorText', () => {
  it('uses the stale-view message for RequestFailed', () => {
    expect(dcuErrorText({
      code: -32803,
      message: 'compiled virtual document is not current or authorized',
    })).toBe(STALE_VIEW_TEXT);
  });

  it('sanitizes closing braces in other error messages', () => {
    expect(dcuErrorText(new Error('boom}')))
      .toBe('{ Compiled view unavailable: boom) }');
  });

  it('formats non-Error values', () => {
    expect(dcuErrorText('x')).toBe('{ Compiled view unavailable: x }');
  });
});
