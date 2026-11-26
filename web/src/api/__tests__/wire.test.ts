import { describe, expect, it } from 'vitest';
import {
  INTERNED_FIELDS,
  UnknownInternedReferenceError,
  normalizeMountAccess,
  resolveSessionInterned,
  serializeAdditionalMount,
  type AdditionalMountWire,
  type WireSessionRow,
} from '../types';
import { liveRow } from '../../test-utils';

describe('resolveSessionInterned', () => {
  const interned = {
    aaaaaaaaaaaaaaaa: [{ name: 'compact', description: 'Compact', source: 'mj' }],
    bbbbbbbbbbbbbbbb: { open: true, prompt: true },
    cccccccccccccccc: [{ key: 'model', label: 'Model', choices: [] }],
    dddddddddddddddd: ['t2'],
    eeeeeeeeeeeeeeee: ['t3'],
  };

  it('expands all five *_ref fields and removes the refs', () => {
    const { available_commands, capabilities, config_options, compatible_resume_targets, incompatible_resume_targets, ...rest } =
      liveRow('s1');
    void available_commands, capabilities, config_options, compatible_resume_targets, incompatible_resume_targets;
    const wire = {
      ...rest,
      available_commands_ref: 'aaaaaaaaaaaaaaaa',
      capabilities_ref: 'bbbbbbbbbbbbbbbb',
      config_options_ref: 'cccccccccccccccc',
      compatible_resume_targets_ref: 'dddddddddddddddd',
      incompatible_resume_targets_ref: 'eeeeeeeeeeeeeeee',
    } as unknown as WireSessionRow;

    const row = resolveSessionInterned(wire, interned) as unknown as Record<string, unknown>;

    expect(row.available_commands).toEqual(interned.aaaaaaaaaaaaaaaa);
    expect(row.capabilities).toEqual(interned.bbbbbbbbbbbbbbbb);
    expect(row.config_options).toEqual(interned.cccccccccccccccc);
    expect(row.compatible_resume_targets).toEqual(['t2']);
    expect(row.incompatible_resume_targets).toEqual(['t3']);
    for (const field of INTERNED_FIELDS) expect(row).not.toHaveProperty(`${field}_ref`);
    expect(row.id).toBe('s1');
  });

  it('leaves inline values alone and does not mutate the input row', () => {
    const wire = liveRow('s1', { available_commands: [] }) as unknown as WireSessionRow;
    const before = structuredClone(wire);
    const row = resolveSessionInterned(wire, new Map());
    expect(row).toEqual(before);
    expect(wire).toEqual(before);
  });

  it('reports an unknown key as UnknownInternedReferenceError so the stream asks for a fresh snapshot', () => {
    const wire = { ...liveRow('s1'), capabilities_ref: 'ffffffffffffffff' } as unknown as WireSessionRow;
    expect(() => resolveSessionInterned(wire, interned)).toThrow(UnknownInternedReferenceError);
  });

  it('refuses a row that carries both a field and its ref', () => {
    const wire = { ...liveRow('s1'), capabilities_ref: 'bbbbbbbbbbbbbbbb' } as unknown as WireSessionRow;
    expect(() => resolveSessionInterned(wire, interned)).toThrow(/both capabilities and capabilities_ref/);
  });
});

describe('AdditionalMount wire format', () => {
  const base = { source: '/src', destination: '/dst' };

  it('writes the exact AdditionalMountRepr for each access mode', () => {
    expect(serializeAdditionalMount({ ...base, access: 'ro' })).toStrictEqual({ ...base, read_only: true });
    expect(serializeAdditionalMount({ ...base, access: 'cow' })).toStrictEqual({ ...base, read_only: false });
    expect(serializeAdditionalMount({ ...base, access: 'rw' })).toStrictEqual({
      ...base,
      read_only: false,
      access: 'rw',
    });
  });

  it('reads access ?? (read_only ? ro : cow)', () => {
    const cases: [AdditionalMountWire, string][] = [
      [{ ...base, read_only: true }, 'ro'],
      [{ ...base, read_only: false }, 'cow'],
      [{ ...base }, 'cow'],
      [{ ...base, read_only: false, access: 'rw' }, 'rw'],
      [{ ...base, read_only: true, access: 'cow' }, 'cow'],
    ];
    for (const [wire, access] of cases) expect(normalizeMountAccess(wire).access).toBe(access);
  });

  it('round-trips through the wire without gaining or losing fields', () => {
    for (const access of ['ro', 'cow', 'rw'] as const) {
      const mount = { ...base, access };
      expect(normalizeMountAccess(serializeAdditionalMount(mount))).toStrictEqual(mount);
    }
  });

  it('accepts a wire-shaped mount when serializing', () => {
    expect(serializeAdditionalMount({ ...base, read_only: true })).toStrictEqual({ ...base, read_only: true });
  });
});
