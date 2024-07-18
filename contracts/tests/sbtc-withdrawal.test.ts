import {
  alice,
  bob,
  deployer,
  deposit,
  errors,
  registry,
  stxAddressToPoxAddress,
  token,
  withdrawal,
} from "./helpers";
import { test, expect, describe } from "vitest";
import { txOk, filterEvents, rov, txErr, rovOk, rovErr } from "@clarigen/test";
import { CoreNodeEventType, cvToValue } from "@clarigen/core";

const alicePoxAddr = stxAddressToPoxAddress(alice);

function newPoxAddr(version: number, hashbytes: Uint8Array) {
  return {
    version: new Uint8Array([version]),
    hashbytes,
  };
}

describe("Validating recipient address", () => {
  test("Should be valid for all different address types", () => {
    function expectValidAddr(bytesLen: number, version: number) {
      const recipient = newPoxAddr(version, new Uint8Array(bytesLen).fill(0));
      expect(rovOk(withdrawal.validateRecipient(recipient))).toEqual(true);
    }
    expectValidAddr(20, 0);
    expectValidAddr(20, 1);
    expectValidAddr(20, 2);
    expectValidAddr(20, 3);
    expectValidAddr(20, 4);
    expectValidAddr(32, 5);
    expectValidAddr(32, 6);
  });

  test("should not support incorrect versions", () => {
    expect(
      rovErr(withdrawal.validateRecipient(newPoxAddr(7, new Uint8Array(32))))
    ).toEqual(errors.withdrawal.ERR_INVALID_ADDR_VERSION);
    expect(
      rovErr(withdrawal.validateRecipient(newPoxAddr(8, new Uint8Array(32))))
    ).toEqual(errors.withdrawal.ERR_INVALID_ADDR_VERSION);
  });

  test("should not support incorrect byte lengths", async () => {
    function expectInvalidAddr(bytesLen: number, version: number) {
      const recipient = newPoxAddr(version, new Uint8Array(bytesLen).fill(0));
      expect(rovErr(withdrawal.validateRecipient(recipient))).toEqual(
        errors.withdrawal.ERR_INVALID_ADDR_HASHBYTES
      );
    }
    // Test a bunch of lengths other than 20
    for (let i = 0; i < 34; i++) {
      if (i === 20) continue;
      for (let v = 0; v <= 4; v++) {
        expectInvalidAddr(i, v);
      }
    }
    // Test a bunch of lengths other than 32
    for (let i = 0; i < 50; i++) {
      if (i === 32) continue;
      for (let v = 5; v <= 6; v++) {
        expectInvalidAddr(i, v);
      }
    }
  });
});

describe("initiating a withdrawal request", () => {
  test("alice can initiate a request", () => {
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1001n,
        recipient: alice,
      }),
      deployer
    );
    const receipt = txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );

    expect(receipt.value).toEqual(1n);

    // The request was stored correctly

    const request = rov(registry.getWithdrawalRequest(1n));
    if (!request) {
      throw new Error("Request not stored");
    }
    expect(request).toStrictEqual({
      sender: alice,
      recipient: alicePoxAddr,
      amount: 1000n,
      maxFee: 10n,
      blockHeight: 2n,
      status: null,
    });

    // An event is emitted properly
    const prints = filterEvents(
      receipt.events,
      CoreNodeEventType.ContractEvent
    );
    expect(prints.length).toEqual(1);
    const [print] = prints;
    const printData = cvToValue<{
      sender: string;
      recipient: { version: Uint8Array; hashbytes: Uint8Array };
      amount: bigint;
      maxFee: bigint;
      blockHeight: bigint;
      topic: string;
    }>(print.data.value);

    expect(printData).toStrictEqual({
      sender: alice,
      recipient: alicePoxAddr,
      amount: 1000n,
      maxFee: 10n,
      blockHeight: 2n,
      topic: "withdrawal-request",
      requestId: 1n,
    });
  });

  test("Tokens are converted to locked sBTC", () => {
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    expect(rovOk(token.getBalance(alice))).toEqual(1000n);
    const receipt = txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    const lockedBalance = rovOk(token.getBalanceLocked(alice));
    expect(lockedBalance).toEqual(1000n);
    const [mintEvent] = filterEvents(
      receipt.events,
      CoreNodeEventType.FtMintEvent
    );
    expect(mintEvent.data.asset_identifier).toEqual(
      `${token.identifier}::${token.fungible_tokens[1].name}`
    );
    expect(mintEvent.data.amount).toEqual(1000n.toString());
    expect(rovOk(token.getBalanceAvailable(alice))).toEqual(0n);
  });

  test("Recipient is validated when initiating an address", () => {
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 4000n,
        recipient: alice,
      }),
      deployer
    );
    expect(
      txErr(
        withdrawal.initiateWithdrawalRequest({
          amount: 1000n,
          recipient: newPoxAddr(7, new Uint8Array(32)),
          maxFee: 10n,
        }),
        alice
      ).value
    ).toEqual(errors.withdrawal.ERR_INVALID_ADDR_VERSION);

    expect(
      txErr(
        withdrawal.initiateWithdrawalRequest({
          amount: 1000n,
          recipient: newPoxAddr(2, new Uint8Array(32)),
          maxFee: 10n,
        }),
        alice
      ).value
    ).toEqual(errors.withdrawal.ERR_INVALID_ADDR_HASHBYTES);

    expect(
      txErr(
        withdrawal.initiateWithdrawalRequest({
          amount: 1000n,
          recipient: newPoxAddr(6, new Uint8Array(20)),
          maxFee: 10n,
        }),
        alice
      ).value
    ).toEqual(errors.withdrawal.ERR_INVALID_ADDR_HASHBYTES);
  });

  test("withdrawal amount of less than or equal to dust limit is rejected", () => {
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 4000n,
        recipient: alice,
      }),
      deployer
    );
    const receipt = txErr(
      withdrawal.initiateWithdrawalRequest({
        amount: withdrawal.constants.DUST_LIMIT,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    expect(receipt.value).toEqual(errors.withdrawal.ERR_DUST_LIMIT);
  });
});

describe("Accepting a withdrawal request", () => {
  test("Fails with non-existant request-id", () => {
    // Alice initiates withdrawalrequest
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    const receipt = txErr(
      withdrawal.acceptWithdrawalRequest({
        requestId: 2n,
        bitcoinTxid: new Uint8Array(32).fill(0),
        signerBitmap: 0n,
        outputIndex: 10n,
        fee: 1n,
      }),
      deployer
    );
    expect(receipt.value).toEqual(errors.withdrawal.ERR_INVALID_REQUEST);
  });
  test("Fails when called by non-signer", () => {
    // Alice initiates withdrawalrequest
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    const receipt = txErr(
      withdrawal.acceptWithdrawalRequest({
        requestId: 1n,
        bitcoinTxid: new Uint8Array(32).fill(0),
        signerBitmap: 0n,
        outputIndex: 10n,
        fee: 1n,
      }),
      alice
    );
    expect(receipt.value).toEqual(errors.withdrawal.ERR_INVALID_CALLER);
  });
  test("Fails when replay is attempted", () => {
    // Alice initiates withdrawalrequest
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    txOk(
      withdrawal.acceptWithdrawalRequest({
        requestId: 1n,
        bitcoinTxid: new Uint8Array(32).fill(0),
        signerBitmap: 0n,
        outputIndex: 10n,
        fee: 1n,
      }),
      deployer
    );
    const receipt = txErr(
      withdrawal.acceptWithdrawalRequest({
        requestId: 1n,
        bitcoinTxid: new Uint8Array(32).fill(0),
        signerBitmap: 0n,
        outputIndex: 10n,
        fee: 1n,
      }),
      deployer
    );
    expect(receipt.value).toEqual(errors.withdrawal.ERR_ALREADY_PROCESSED);
  });
  test("Fails when fee is too high", () => {
    // Alice initiates withdrawalrequest
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    const receipt = txErr(
      withdrawal.acceptWithdrawalRequest({
        requestId: 1n,
        bitcoinTxid: new Uint8Array(32).fill(0),
        signerBitmap: 0n,
        outputIndex: 10n,
        fee: 11n,
      }),
      deployer
    );
    expect(receipt.value).toEqual(errors.withdrawal.ERR_FEE_TOO_HIGH);
  });
  test("Request is successfully accepted with max fee", () => {
    // Alice initiates withdrawalrequest
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    txOk(
      withdrawal.acceptWithdrawalRequest({
        requestId: 1n,
        bitcoinTxid: new Uint8Array(32).fill(0),
        signerBitmap: 0n,
        outputIndex: 10n,
        fee: 10n,
      }),
      deployer
    );
    expect(rovOk(token.getBalance(alice))).toEqual(0n);
  });
  test("Request is successfully accepted with fee less than max", () => {
    // Alice initiates withdrawalrequest
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    txOk(
      withdrawal.acceptWithdrawalRequest({
        requestId: 1n,
        bitcoinTxid: new Uint8Array(32).fill(0),
        signerBitmap: 0n,
        outputIndex: 10n,
        fee: 9n,
      }),
      deployer
    );
    expect(rovOk(token.getBalance(alice))).toEqual(1n);
  });
});

describe("Reject a withdrawal request", () => {
  test("Fails with non-existant request-id", () => {
    // Alice initiates withdrawalrequest
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    const receipt = txErr(
      withdrawal.rejectWithdrawalRequest({
        requestId: 2n,
        signerBitmap: 0n,
      }),
      alice
    );
    expect(receipt.value).toEqual(errors.withdrawal.ERR_INVALID_REQUEST);
  });
  test("Fails when called by a non-signer", () => {
    // Alice initiates withdrawalrequest
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    const receipt = txErr(
      withdrawal.rejectWithdrawalRequest({
        requestId: 1n,
        signerBitmap: 0n,
      }),
      alice
    );
    expect(receipt.value).toEqual(errors.withdrawal.ERR_INVALID_CALLER);
  });
  test("Fails when request id is replayed", () => {
    // Alice initiates withdrawalrequest
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    txOk(
      withdrawal.acceptWithdrawalRequest({
        requestId: 1n,
        bitcoinTxid: new Uint8Array(32).fill(0),
        signerBitmap: 0n,
        outputIndex: 10n,
        fee: 10n,
      }),
      deployer
    );
    const receipt = txErr(
      withdrawal.rejectWithdrawalRequest({
        requestId: 1n,
        signerBitmap: 0n,
      }),
      deployer
    );
    expect(receipt.value).toEqual(errors.withdrawal.ERR_ALREADY_PROCESSED);
  });
  test("Successfully reject a requested withdrawal", () => {
    // Alice initiates withdrawalrequest
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    const receipt = txOk(
      withdrawal.acceptWithdrawalRequest({
        requestId: 1n,
        bitcoinTxid: new Uint8Array(32).fill(0),
        signerBitmap: 0n,
        outputIndex: 10n,
        fee: 10n,
      }),
      deployer
    );
    expect(receipt.value).toEqual(true);
  });
});

describe("Complete multiple withdrawals", () => {
  test("Successfully pass in two withdrawals, one accept, one reject", () => {
    // Alice setup
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(0),
        voutIndex: 0,
        amount: 1000n,
        recipient: alice,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      alice
    );
    // Bob setup
    txOk(
      deposit.completeDepositWrapper({
        txid: new Uint8Array(32).fill(1),
        voutIndex: 1,
        amount: 1000n,
        recipient: bob,
      }),
      deployer
    );
    txOk(
      withdrawal.initiateWithdrawalRequest({
        amount: 1000n,
        recipient: alicePoxAddr,
        maxFee: 10n,
      }),
      bob
    );
    //
    const receipt = txOk(
      withdrawal.completeWithdrawals({
        withdrawals: [
          {
            requestId: 1n,
            status: true,
            signerBitmap: 1n,
            bitcoinTxid: new Uint8Array(32).fill(1),
            outputIndex: 10n,
            fee: 10n,
          },
          {
            requestId: 2n,
            status: false,
            signerBitmap: 1n,
            bitcoinTxid: null,
            outputIndex: null,
            fee: null,
          },
        ],
      }),
      deployer
    );
    expect(receipt.value).toEqual(2n);
  });
});
