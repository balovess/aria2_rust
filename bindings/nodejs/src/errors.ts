export class Aria2Error extends Error {
  public readonly code: number;

  constructor(message: string, code: number = -1) {
    super(message);
    this.name = 'Aria2Error';
    this.code = code;
  }
}

export class ConnectionError extends Aria2Error {
  constructor(message: string) {
    super(message, -2);
    this.name = 'ConnectionError';
  }
}

export class AuthError extends Aria2Error {
  constructor(message: string) {
    super(message, -3);
    this.name = 'AuthError';
  }
}

export class RpcError extends Aria2Error {
  constructor(message: string, code: number) {
    super(message, code);
    this.name = 'RpcError';
  }
}

export class TimeoutError extends Aria2Error {
  constructor(message: string) {
    super(message, -4);
    this.name = 'TimeoutError';
  }
}
