//
// Copyright 2021 Signal Messenger, LLC.
// SPDX-License-Identifier: AGPL-3.0-only
//

export class UntrustedIdentityError extends Error {
  constructor(public readonly addr: string) {
    super('untrusted identity for address ' + addr);
  }
}

export class SealedSenderSelfSend extends Error {
  constructor(message: string) {
    super(message);
  }
}
