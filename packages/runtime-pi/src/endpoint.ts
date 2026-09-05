import { isIP } from "node:net";

// Pi starts independently of core, so validate its bind endpoint here too.
export function runtimeListenAddress(value: string) {
  const url = new URL(value);
  const host = url.hostname.replace(/^\[|\]$/gu, "");
  // WHATWG URLs omit an explicit HTTP default port from .port.
  const port = Number(url.port || "80");
  if (
    !/^http:\/\/(?:\[[^\]]+\]|[^:/@?#]+):\d+$/u.test(value) ||
    !((isIP(host) === 4 && host.startsWith("127.")) || host === "::1") ||
    port === 0
  )
    throw new Error("runtime URL must use an HTTP loopback socket address and a nonzero port");
  return { host, port };
}
