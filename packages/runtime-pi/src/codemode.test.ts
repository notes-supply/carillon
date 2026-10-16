import {
  ModelRuntime,
  SessionManager,
  type AgentSessionEvent,
} from "@earendil-works/pi-coding-agent";
import {
  createAssistantMessageEventStream,
  type AssistantMessage,
  type Model,
} from "@earendil-works/pi-ai";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, it } from "vitest";
import { createPiExecutionSession, openConversation, promptSession } from "@carillon/runtime-pi";

const model: Model<"openai-responses"> = {
  id: "codemode-observer",
  name: "Codemode observer",
  api: "openai-responses",
  provider: "openai",
  baseUrl: "https://example.invalid",
  reasoning: false,
  input: ["text"],
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 32_000,
  maxTokens: 2_000,
};

// Only inference is substituted. Pi executes the script, workers, nested tools,
// extension hooks, cancellation and session persistence itself.
async function scriptedRuntime(scripts: string[]) {
  const runtime = await ModelRuntime.create({
    allowModelNetwork: false,
    refreshOnCreate: false,
    modelsPath: null,
  });
  let nextScript = true;
  return new Proxy(runtime, {
    get(target, property): unknown {
      if (property === "getAuth")
        return () => Promise.resolve({ auth: { apiKey: "fixture-only" } });
      if (property === "hasConfiguredAuth") return () => true;
      if (property === "streamSimple")
        return () => {
          const code = nextScript ? scripts.shift() : undefined;
          nextScript = !nextScript;
          const message: AssistantMessage = {
            role: "assistant",
            api: model.api,
            provider: model.provider,
            model: model.id,
            timestamp: Date.now(),
            usage: {
              input: 0,
              output: 0,
              cacheRead: 0,
              cacheWrite: 0,
              totalTokens: 0,
              cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
            },
            stopReason: code === undefined ? "stop" : "toolUse",
            content:
              code === undefined
                ? [{ type: "text", text: "checked" }]
                : [
                    {
                      type: "toolCall",
                      id: `script-${String(scripts.length)}`,
                      name: "codemode",
                      arguments: { code },
                    },
                  ],
          };
          const stream = createAssistantMessageEventStream();
          queueMicrotask(() =>
            stream.push({ type: "done", reason: code === undefined ? "stop" : "toolUse", message }),
          );
          return stream;
        };
      const value: unknown = Reflect.get(target, property, target);
      return typeof value === "function" ? value.bind(target) : value;
    },
  });
}

it.each(["on", "only"] as const)(
  "runs native codemode tools/hooks and reopens its store (%s)",
  async (mode) => {
    const root = await mkdtemp(join(tmpdir(), "carillon-codemode-"));
    const key = { interface_id: "discord", agent_id: "default", audience_id: "code-mode" };
    await writeFile(
      join(root, "settings.json"),
      JSON.stringify({ defaultTools: ["+codemode"], codemode: { mode } }),
    );
    await mkdir(join(root, "extensions"));
    await writeFile(
      join(root, "extensions", "guard.ts"),
      `export default function guard(pi) {
  pi.on("tool_call", (event) => {
    if (event.toolName === "bash" && event.input.command.includes("blocked.txt"))
      return { block: true, reason: "native-hook-block" };
  });
}`,
    );
    const scripts = [
      `await tools.write({path: "result.txt", content: "before"});
await tools.edit({path: "result.txt", edits: [{oldText: "before", newText: "after"}]});
const results = await Promise.allSettled([tools.read({path: "result.txt"}), tools.bash({command: "printf native-bash"}), tools.bash({command: "printf forbidden > blocked.txt"})]);
text(results); store("sentinel", "survives-reopen");`,
      `text(load("sentinel")); store("failed-write", "must-not-persist"); throw new Error("intentional-script-failure");`,
      `text({sentinel: load("sentinel"), failed: load("failed-write") ?? null});`,
    ];
    const runtime = await scriptedRuntime(scripts);
    const events: AgentSessionEvent[] = [];
    const open = async () =>
      (
        await createPiExecutionSession({
          resourceRoot: root,
          workspace: root,
          manager: openConversation(root, join(root, "sessions"), key),
          modelRuntime: runtime,
          model,
        })
      ).session;
    try {
      const first = await open();
      try {
        expect(first.getActiveToolNames()).toContain("codemode");
        const unsubscribe = first.subscribe((event) => events.push(event));
        try {
          await promptSession(first, "run script", new AbortController().signal);
        } finally {
          unsubscribe();
        }
        expect(await readFile(join(root, "result.txt"), "utf8")).toBe("after");
        const result = first.messages.find(
          (message) => message.role === "toolResult" && message.toolName === "codemode",
        );
        expect(JSON.stringify(result)).toContain("native-bash");
        expect(JSON.stringify(result)).toContain("after");
        expect(JSON.stringify(result)).toContain("native-hook-block");
        await expect(readFile(join(root, "blocked.txt"))).rejects.toMatchObject({ code: "ENOENT" });
        expect(
          events
            .filter(
              (event) =>
                event.type === "tool_execution_start" && event.parentToolCallId !== undefined,
            )
            .map((event) => (event.type === "tool_execution_start" ? event.toolName : "")),
        ).toEqual(["write", "edit", "read", "bash", "bash"]);
      } finally {
        first.dispose();
      }
      const reopened = await open();
      try {
        await promptSession(reopened, "fail a script", new AbortController().signal);
        expect(JSON.stringify(reopened.messages)).toContain("intentional-script-failure");
        await promptSession(reopened, "continue", new AbortController().signal);
        const result = reopened.messages.filter((message) => message.role === "toolResult").at(-1);
        expect(JSON.stringify(result)).toContain("survives-reopen");
        if (result?.role !== "toolResult") throw new Error("missing codemode result");
        expect(
          result.content
            .filter((block) => block.type === "text")
            .map((block) => block.text)
            .join("\n"),
        ).toContain('"failed":null');
        expect(reopened.isIdle).toBe(true);
      } finally {
        reopened.dispose();
      }
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  },
  20_000,
);

it("leaves native codemode inactive unless selected and honors builtin disable", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-codemode-disabled-"));
  const runtime = await scriptedRuntime([]);
  try {
    for (const settings of [
      {},
      { defaultTools: ["+codemode"], extensions: ["-builtin:codemode"] },
    ]) {
      await writeFile(join(root, "settings.json"), JSON.stringify(settings));
      const manager = SessionManager.inMemory(root);
      const { session, extensionsResult } = await createPiExecutionSession({
        resourceRoot: root,
        workspace: root,
        manager,
        modelRuntime: runtime,
        model,
      });
      try {
        expect(extensionsResult.errors).toEqual([]);
        expect(session.getActiveToolNames()).not.toContain("codemode");
        expect(session.getActiveToolNames()).toEqual(
          expect.arrayContaining(["read", "bash", "edit", "write"]),
        );
      } finally {
        session.dispose();
      }
    }
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

it("cancels a native codemode child and settles before reopening", async () => {
  const root = await mkdtemp(join(tmpdir(), "carillon-codemode-cancel-"));
  const key = { interface_id: "discord", agent_id: "default", audience_id: "cancel" };
  const runtime = await scriptedRuntime([
    `await tools.bash({command: "printf started > started; sleep 30; printf finished > finished"});`,
  ]);
  await writeFile(join(root, "settings.json"), JSON.stringify({ defaultTools: ["+codemode"] }));
  const { session } = await createPiExecutionSession({
    resourceRoot: root,
    workspace: root,
    manager: openConversation(root, join(root, "sessions"), key),
    modelRuntime: runtime,
    model,
  });
  const abort = new AbortController();
  try {
    const running = promptSession(session, "run script", abort.signal);
    const rejected = running.then(
      () => undefined,
      (error: unknown) => error,
    );
    await expect
      .poll(() => readFile(join(root, "started"), "utf8").catch(() => ""), { timeout: 8_000 })
      .toBe("started");
    abort.abort();
    expect(await rejected).toMatchObject({ name: "AbortError" });
    expect(session.isIdle).toBe(true);
    await expect(readFile(join(root, "finished"))).rejects.toMatchObject({ code: "ENOENT" });
    session.dispose();
    const { session: next } = await createPiExecutionSession({
      resourceRoot: root,
      workspace: root,
      manager: openConversation(root, join(root, "sessions"), key),
      modelRuntime: runtime,
      model,
    });
    try {
      await promptSession(next, "continue after cancellation", new AbortController().signal);
      expect(next.isIdle).toBe(true);
      expect(next.messages.filter((message) => message.role === "user")).toHaveLength(2);
    } finally {
      next.dispose();
    }
  } finally {
    await session.abort();
    session.dispose();
    await rm(root, { recursive: true, force: true });
  }
});
