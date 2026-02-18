// Wallet connection abstraction.
//
// Supports two sources:
//   1. Keplr browser extension (production path)
//   2. Raw secp256k1 private key via DirectSecp256k1Wallet (dev/testing)
//
// Both return an OfflineDirectSigner with identical getAccounts()/signDirect()
// interfaces, so the rest of the signing pipeline (cosmosTx.ts) is agnostic.

import { DirectSecp256k1Wallet } from "@cosmjs/proto-signing";
import type { OfflineDirectSigner } from "@cosmjs/proto-signing";
import { fromHex } from "@cosmjs/encoding";
import type { KeplrChainInfo } from "../types/keplr";

const BECH32_PREFIX = "zvote";

const COIN = {
  coinDenom: "ZVOTE",
  coinMinimalDenom: "uzvote",
  coinDecimals: 6,
};

export interface WalletConnection {
  signer: OfflineDirectSigner;
  address: string;
}

async function fetchChainId(restUrl: string): Promise<string> {
  const resp = await fetch(`${restUrl}/cosmos/base/tendermint/v1beta1/node_info`);
  if (!resp.ok) {
    throw new Error(`Failed to fetch chain ID: HTTP ${resp.status}`);
  }
  const data = await resp.json();
  return data.default_node_info?.network ?? "";
}

function buildChainInfo(chainId: string, restUrl: string, rpcUrl: string): KeplrChainInfo {
  return {
    chainId,
    chainName: "Zally Voting",
    rpc: rpcUrl,
    rest: restUrl,
    bip44: { coinType: 133 },
    bech32Config: {
      bech32PrefixAccAddr: BECH32_PREFIX,
      bech32PrefixAccPub: `${BECH32_PREFIX}pub`,
      bech32PrefixValAddr: `${BECH32_PREFIX}valoper`,
      bech32PrefixValPub: `${BECH32_PREFIX}valoperpub`,
      bech32PrefixConsAddr: `${BECH32_PREFIX}valcons`,
      bech32PrefixConsPub: `${BECH32_PREFIX}valconspub`,
    },
    currencies: [COIN],
    feeCurrencies: [
      {
        ...COIN,
        gasPriceStep: { low: 0, average: 0, high: 0 },
      },
    ],
    stakeCurrency: COIN,
    features: [],
  };
}

/**
 * Connect via the Keplr browser extension.
 *
 * `restUrl` should be the fully-qualified chain REST URL (e.g. http://localhost:1318).
 * For dev-mode proxy, pass the origin (window.location.origin) so that Keplr can
 * reach the node. `rpcUrl` is the Tendermint RPC endpoint (e.g. http://localhost:26657).
 */
export async function connectKeplr(restUrl: string, rpcUrl: string): Promise<WalletConnection> {
  if (!window.keplr) {
    throw new Error("Keplr extension not found. Please install Keplr to connect your wallet.");
  }

  const chainId = await fetchChainId(restUrl);
  if (!chainId) {
    throw new Error("Could not determine chain ID from the node.");
  }

  const chainInfo = buildChainInfo(chainId, restUrl, rpcUrl);
  await window.keplr.experimentalSuggestChain(chainInfo);
  await window.keplr.enable(chainId);

  const signer = window.keplr.getOfflineSigner(chainId);
  const [account] = await signer.getAccounts();

  return { signer, address: account.address };
}

/**
 * Connect using a raw hex-encoded secp256k1 private key.
 * Intended for local development against a test chain.
 */
export async function connectWithPrivateKey(privateKeyHex: string): Promise<WalletConnection> {
  const privkey = fromHex(privateKeyHex);
  const signer = await DirectSecp256k1Wallet.fromKey(privkey, BECH32_PREFIX);
  const [account] = await signer.getAccounts();
  return { signer, address: account.address };
}
