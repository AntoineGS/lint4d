import { describe, expect, it } from 'vitest';
import { capabilityFlags } from '../../src/protocol';

const allDisabled = {
  projectSelection: false,
  projectCatalogue: false,
  buildSelection: false,
  installationSelection: false,
};

describe('capabilityFlags', () => {
  it('treats missing experimental capabilities as all disabled', () => {
    expect(capabilityFlags(undefined)).toEqual(allDisabled);
  });

  it('enables only flags whose value is exactly true', () => {
    expect(capabilityFlags({ projectSelection: true, buildSelection: 'yes' })).toEqual({
      projectSelection: true,
      projectCatalogue: false,
      buildSelection: false,
      installationSelection: false,
    });
  });

  it('treats null experimental capabilities as all disabled', () => {
    expect(capabilityFlags(null)).toEqual(allDisabled);
  });
});
