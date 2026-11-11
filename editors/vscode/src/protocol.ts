export const EXPERIMENTAL_FLAGS = [
  'projectSelection',
  'projectCatalogue',
  'buildSelection',
  'installationSelection',
] as const;

export type ExperimentalFlag = (typeof EXPERIMENTAL_FLAGS)[number];

export function capabilityFlags(experimental: unknown): Record<ExperimentalFlag, boolean> {
  const source = typeof experimental === 'object' && experimental !== null
    ? experimental as Record<string, unknown>
    : {};
  return Object.fromEntries(EXPERIMENTAL_FLAGS.map((flag) => [flag, source[flag] === true])) as Record<
    ExperimentalFlag,
    boolean
  >;
}
