import assert from "node:assert/strict";
import { once } from "node:events";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";

export const PORT_OFFSETS = { core: 0, discord: 1, runtime: 2, "runtime-t3": 3, web: 4 };

// The probe releases the ports before the services bind, so the block lies
// below the kernel's ephemeral range, where outgoing connections can't take a
// port in between, and below the 128 ports under it that the installer and
// persistence tests take, which may run at the same time.
export async function availableBasePort() {
  const range = await readFile("/proc/sys/net/ipv4/ip_local_port_range", "utf8");
  const ceiling = Number(range.trim().split(/\s+/)[0]) - 128;
  assert.ok(ceiling > 1028, `no five free ports below the ephemeral range ${range.trim()}`);
  for (let attempt = 0; attempt < 10; attempt += 1) {
    const port = 1024 + Math.floor(Math.random() * (ceiling - 1028));
    const listeners = [];
    try {
      for (const offset of Object.values(PORT_OFFSETS)) {
        const next = createServer().listen(port + offset, "127.0.0.1");
        listeners.push(next);
        await once(next, "listening");
      }
      return port;
    } catch (error) {
      if (error.code !== "EADDRINUSE") throw error;
    } finally {
      await Promise.all(listeners.map((server) => new Promise((resolve) => server.close(resolve))));
    }
  }
  throw new Error("could not find five available integration service ports");
}
