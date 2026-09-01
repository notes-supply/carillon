import { SessionManager } from "@earendil-works/pi-coding-agent";
import type { AssistantMessage } from "@earendil-works/pi-ai";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, it } from "vitest";
import { capabilityCallId, nativeSessionId } from "./namespace.ts";

it("namespaces capability operations by turn and native tool call", () => {
  expect(capabilityCallId("turn-a", "call-1")).toBe("turn-a:call-1");
  expect(capabilityCallId("turn-a", "call-1")).not.toBe(capabilityCallId("turn-a", "call-2"));
  expect(capabilityCallId("turn-a", "call-1")).not.toBe(capabilityCallId("turn-b", "call-1"));
});

function assistantMessage(text: string, timestamp: number): AssistantMessage {
  return {
    role: "assistant",
    content: [{ type: "text", text }],
    api: "openai-responses",
    provider: "provider",
    model: "model",
    responseModel: "model",
    responseId: `response-${String(timestamp)}`,
    usage: {
      input: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
      reasoning: 0,
      totalTokens: 0,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
    },
    stopReason: "stop",
    timestamp,
  };
}

it("isolates identical agent and audience identities across interfaces", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-session-namespace-"));
  const sessions = join(root, "sessions");
  await mkdir(sessions);
  try {
    const firstId = nativeSessionId("interface-a", "default", "shared-audience");
    const secondId = nativeSessionId("interface-b", "default", "shared-audience");
    expect(firstId).not.toBe(secondId);

    const first = SessionManager.create(root, sessions, { id: firstId });
    first.appendMessage({ role: "user", content: "interface a context", timestamp: 1 });
    first.appendMessage(assistantMessage("interface a response", 2));
    const firstPath = first.getSessionFile();
    const second = SessionManager.create(root, sessions, { id: secondId });
    second.appendMessage({ role: "user", content: "interface b context", timestamp: 3 });
    second.appendMessage(assistantMessage("interface b response", 4));
    const secondPath = second.getSessionFile();
    if (firstPath === undefined || secondPath === undefined)
      throw new Error("native session paths were not persisted");
    expect(firstPath).not.toBe(secondPath);

    const firstContext = SessionManager.open(firstPath, sessions, root).buildSessionContext();
    const secondContext = SessionManager.open(secondPath, sessions, root).buildSessionContext();
    expect(firstContext.messages).toHaveLength(2);
    expect(firstContext.messages[0]).toMatchObject({
      role: "user",
      content: "interface a context",
    });
    expect(firstContext.messages[1]).toMatchObject({
      role: "assistant",
      content: [{ type: "text", text: "interface a response" }],
    });
    expect(secondContext.messages).toHaveLength(2);
    expect(secondContext.messages[0]).toMatchObject({
      role: "user",
      content: "interface b context",
    });
    expect(secondContext.messages[1]).toMatchObject({
      role: "assistant",
      content: [{ type: "text", text: "interface b response" }],
    });
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
