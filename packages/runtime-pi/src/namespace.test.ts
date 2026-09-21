import type { AssistantMessage } from "@earendil-works/pi-ai";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, it } from "vitest";
import { conversationDirectory, openConversation } from "@carillon/runtime-pi";

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
    const firstKey = {
      interface_id: "interface-a",
      agent_id: "default",
      audience_id: "shared-audience",
    };
    const secondKey = { ...firstKey, interface_id: "interface-b" };
    const firstId = conversationDirectory(sessions, firstKey);
    const secondId = conversationDirectory(sessions, secondKey);
    expect(firstId).not.toBe(secondId);

    const first = openConversation(root, sessions, firstKey);
    first.appendMessage({ role: "user", content: "interface a context", timestamp: 1 });
    first.appendMessage(assistantMessage("interface a response", 2));
    const firstPath = first.getSessionFile();
    const second = openConversation(root, sessions, secondKey);
    second.appendMessage({ role: "user", content: "interface b context", timestamp: 3 });
    second.appendMessage(assistantMessage("interface b response", 4));
    const secondPath = second.getSessionFile();
    if (firstPath === undefined || secondPath === undefined)
      throw new Error("native session paths were not persisted");
    expect(firstPath).not.toBe(secondPath);

    const firstContext = openConversation(root, sessions, firstKey).buildSessionContext();
    const secondContext = openConversation(root, sessions, secondKey).buildSessionContext();
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

it("reopens a first turn's saved tool call without replaying the user message", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-interrupted-session-"));
  const key = { interface_id: "discord", agent_id: "default", audience_id: "thread" };
  try {
    const first = openConversation(root, join(root, "sessions"), key);
    first.appendMessage({ role: "user", content: "first request", timestamp: 1 });
    first.appendMessage({
      ...assistantMessage("", 2),
      stopReason: "toolUse",
      content: [
        {
          type: "toolCall",
          id: "interrupted-tool",
          name: "bash",
          arguments: { command: "example" },
        },
      ],
    });
    const reopened = openConversation(root, join(root, "sessions"), key);
    expect(reopened.getSessionFile()).toBe(first.getSessionFile());
    expect(reopened.getSessionId()).toBe(first.getSessionId());
    expect(reopened.buildSessionContext().messages).toHaveLength(2);
    reopened.appendMessage({ role: "user", content: "new direction", timestamp: 3 });
    const again = openConversation(root, join(root, "sessions"), key);
    expect(
      again.buildSessionContext().messages.filter((message) => message.role === "user"),
    ).toEqual([
      { role: "user", content: "first request", timestamp: 1 },
      { role: "user", content: "new direction", timestamp: 3 },
    ]);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
