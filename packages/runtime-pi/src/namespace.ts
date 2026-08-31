import { createHash } from "node:crypto";

export function nativeSessionId(interfaceId: string, agentId: string, audienceId: string): string {
  const hash = createHash("sha256");
  for (const value of [interfaceId, agentId, audienceId]) hash.update(value).update("\0");
  return hash.digest("hex");
}
