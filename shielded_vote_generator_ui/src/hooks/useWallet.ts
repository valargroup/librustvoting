import { useState, useCallback, useEffect } from "react";
import type { OfflineDirectSigner } from "@cosmjs/proto-signing";
import { connectKeplr, connectWithPrivateKey } from "../api/wallet";
import * as chainApi from "../api/chain";

type WalletSource = "keplr" | "privkey";

const SOURCE_KEY = "zally-wallet-source";

// Tendermint RPC — defaults to same host as REST but on the standard RPC port.
const DEFAULT_RPC_URL = "http://localhost:26657";

function resolveUrls(): { restUrl: string; rpcUrl: string } {
  // In dev mode always use the Vite proxy origin for REST so /cosmos/* paths
  // are forwarded server-side, regardless of any stored chain URL.
  const restUrl = import.meta.env.DEV
    ? window.location.origin
    : chainApi.getChainUrl();

  return { restUrl, rpcUrl: DEFAULT_RPC_URL };
}

export interface UseWallet {
  address: string | null;
  signer: OfflineDirectSigner | null;
  source: WalletSource | null;
  connecting: boolean;
  error: string | null;
  connect: () => Promise<void>;
  connectDev: (privateKeyHex: string) => Promise<void>;
  disconnect: () => void;
}

export function useWallet(): UseWallet {
  const [address, setAddress] = useState<string | null>(null);
  const [signer, setSigner] = useState<OfflineDirectSigner | null>(null);
  const [source, setSource] = useState<WalletSource | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const applyConnection = useCallback(
    (conn: { signer: OfflineDirectSigner; address: string }, src: WalletSource) => {
      setSigner(conn.signer);
      setAddress(conn.address);
      setSource(src);
      setError(null);
      localStorage.setItem(SOURCE_KEY, src);
    },
    [],
  );

  // Keplr connection
  const connect = useCallback(async () => {
    setConnecting(true);
    setError(null);
    try {
      const { restUrl, rpcUrl } = resolveUrls();
      const conn = await connectKeplr(restUrl, rpcUrl);
      applyConnection(conn, "keplr");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setConnecting(false);
    }
  }, [applyConnection]);

  // Dev private key connection
  const connectDev = useCallback(
    async (privateKeyHex: string) => {
      setConnecting(true);
      setError(null);
      try {
        const conn = await connectWithPrivateKey(privateKeyHex);
        applyConnection(conn, "privkey");
      } catch (err) {
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        setConnecting(false);
      }
    },
    [applyConnection],
  );

  const disconnect = useCallback(() => {
    setSigner(null);
    setAddress(null);
    setSource(null);
    setError(null);
    localStorage.removeItem(SOURCE_KEY);
  }, []);

  // Auto-reconnect Keplr on page load if previously connected.
  useEffect(() => {
    const saved = localStorage.getItem(SOURCE_KEY);
    if (saved === "keplr" && window.keplr) {
      connect();
    }
    // Private key connections are not auto-reconnected — the key is not persisted.
  }, [connect]);

  // Re-derive address when user switches Keplr accounts.
  useEffect(() => {
    if (source !== "keplr") return;

    const handler = () => { connect(); };
    window.addEventListener("keplr_keystorechange", handler);
    return () => window.removeEventListener("keplr_keystorechange", handler);
  }, [source, connect]);

  return {
    address,
    signer,
    source,
    connecting,
    error,
    connect,
    connectDev,
    disconnect,
  };
}
