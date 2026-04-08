import { describe, it, expect } from 'vitest';
import {
  Aria2Error,
  ConnectionError,
  AuthError,
  RpcError,
  TimeoutError,
} from '../../src/errors.js';

describe('Aria2Error', () => {
  it('has code and message', () => {
    const err = new Aria2Error('test message', -1);
    expect(err.message).toBe('test message');
    expect(err.code).toBe(-1);
  });

  it('defaults code to -1', () => {
    const err = new Aria2Error('msg');
    expect(err.code).toBe(-1);
  });

  it('sets name to Aria2Error', () => {
    const err = new Aria2Error('msg');
    expect(err.name).toBe('Aria2Error');
  });

  it('is instance of Error', () => {
    const err = new Aria2Error('msg');
    expect(err).toBeInstanceOf(Error);
  });
});

describe('ConnectionError', () => {
  it('has code -2', () => {
    const err = new ConnectionError('conn failed');
    expect(err.code).toBe(-2);
    expect(err.message).toBe('conn failed');
  });

  it('sets name to ConnectionError', () => {
    const err = new ConnectionError('msg');
    expect(err.name).toBe('ConnectionError');
  });

  it('is instance of Aria2Error', () => {
    const err = new ConnectionError('msg');
    expect(err).toBeInstanceOf(Aria2Error);
  });
});

describe('AuthError', () => {
  it('has code -3', () => {
    const err = new AuthError('auth failed');
    expect(err.code).toBe(-3);
    expect(err.message).toBe('auth failed');
  });

  it('sets name to AuthError', () => {
    const err = new AuthError('msg');
    expect(err.name).toBe('AuthError');
  });

  it('is instance of Aria2Error', () => {
    const err = new AuthError('msg');
    expect(err).toBeInstanceOf(Aria2Error);
  });
});

describe('RpcError', () => {
  it('has code from constructor', () => {
    const err = new RpcError('rpc error', 42);
    expect(err.code).toBe(42);
    expect(err.message).toBe('rpc error');
  });

  it('sets name to RpcError', () => {
    const err = new RpcError('msg', 1);
    expect(err.name).toBe('RpcError');
  });

  it('is instance of Aria2Error', () => {
    const err = new RpcError('msg', 1);
    expect(err).toBeInstanceOf(Aria2Error);
  });
});

describe('TimeoutError', () => {
  it('has code -4', () => {
    const err = new TimeoutError('timed out');
    expect(err.code).toBe(-4);
    expect(err.message).toBe('timed out');
  });

  it('sets name to TimeoutError', () => {
    const err = new TimeoutError('msg');
    expect(err.name).toBe('TimeoutError');
  });

  it('is instance of Aria2Error', () => {
    const err = new TimeoutError('msg');
    expect(err).toBeInstanceOf(Aria2Error);
  });
});

describe('Error hierarchy catch', () => {
  it('catches ConnectionError as Aria2Error', () => {
    expect(() => {
      throw new ConnectionError('fail');
    }).toThrow(Aria2Error);
  });

  it('catches AuthError as Aria2Error', () => {
    expect(() => {
      throw new AuthError('fail');
    }).toThrow(Aria2Error);
  });

  it('catches RpcError as Aria2Error', () => {
    expect(() => {
      throw new RpcError('fail', 1);
    }).toThrow(Aria2Error);
  });

  it('catches TimeoutError as Aria2Error', () => {
    expect(() => {
      throw new TimeoutError('fail');
    }).toThrow(Aria2Error);
  });
});
