export interface DcuCapability {
  uriScheme: string;
  contentMethod: string;
}

export const STALE_VIEW_TEXT = '{ This compiled view is no longer current. Navigate to it again from the importing source. }';

const DEFAULT_CONTENT_METHOD = 'textDocument/content';
const REQUEST_FAILED = -32803;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export function dcuCapability(experimental: unknown): DcuCapability | undefined {
  if (!isRecord(experimental)) return undefined;
  const capability = experimental.compiledDcuVirtualDocuments;
  if (!isRecord(capability) || typeof capability.uriScheme !== 'string' || !capability.uriScheme) {
    return undefined;
  }

  const contentMethod = capability.contentMethod === undefined
    ? DEFAULT_CONTENT_METHOD
    : capability.contentMethod;
  if (typeof contentMethod !== 'string' || !contentMethod) return undefined;
  return { uriScheme: capability.uriScheme, contentMethod };
}

export function dcuErrorText(error: unknown): string {
  if (isRecord(error) && error.code === REQUEST_FAILED) return STALE_VIEW_TEXT;

  const message = error instanceof Error
    ? error.message
    : isRecord(error) && 'message' in error
      ? String(error.message)
      : String(error);
  return `{ Compiled view unavailable: ${message.replaceAll('}', ')')} }`;
}
