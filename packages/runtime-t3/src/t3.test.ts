import { afterEach, beforeEach, expect, it } from "vitest";
import { startFakeT3, type FakeT3 } from "./fake.ts";
import { connectT3, T3Failure } from "./t3.ts";

let fake: FakeT3;

beforeEach(async () => {
  fake = await startFakeT3();
});

afterEach(async () => {
  await fake.close();
});

it("checks the protocol before sending the credential", async () => {
  fake.protocolVersion = 1;
  await expect(connectT3({ url: fake.url, token: fake.token })).rejects.toMatchObject({
    kind: "incompatible",
  });
  await expect(connectT3({ url: "http://127.0.0.1:1", token: fake.token })).rejects.toMatchObject({
    kind: "unreachable",
  });
  fake.protocolVersion = 2;
  await expect(connectT3({ url: fake.url, token: "other" })).rejects.toMatchObject({
    kind: "refused",
  });
});

it("classifies rejections, unknown methods and a closed socket", async () => {
  const connection = await connectT3({ url: fake.url, token: fake.token });
  await expect(connection.call("no.such.method", {})).rejects.toMatchObject({
    kind: "rejected",
    detail: "RpcNotFound from the fake T3: no.such.method",
  });
  const stream = connection.subscribe("orchestration.subscribeThread", { threadId: "none" }, () => {
    throw new Error("no items expected");
  });
  await expect(stream.done).rejects.toMatchObject({
    kind: "rejected",
    detail:
      "OrchestrationV2GetThreadProjectionError from the fake T3: Failed to load orchestration V2 thread none",
  });
  const pending = connection.call("orchestration.dispatchCommand", {
    type: "thread.create",
    commandId: "c",
    threadId: "t",
    projectId: "p",
  });
  fake.delayAnswer = () => 10_000;
  fake.disconnectAll();
  await expect(pending).rejects.toMatchObject({ kind: "disconnected" });
  await connection.closed;
  await expect(connection.call("orchestration.dispatchCommand", {})).rejects.toBeInstanceOf(
    T3Failure,
  );
});

it("delivers stream items and ends a stream on request", async () => {
  const connection = await connectT3({ url: fake.url, token: fake.token });
  await connection.call("orchestration.dispatchCommand", {
    type: "thread.create",
    commandId: "create",
    threadId: "thread",
    projectId: "project",
  });
  const items: unknown[] = [];
  const stream = connection.subscribe(
    "orchestration.subscribeThread",
    { threadId: "thread" },
    (item) => items.push(item),
  );
  await fake.waitFor(() => items.length === 1);
  expect(items[0]).toMatchObject({ kind: "snapshot", projection: { thread: { id: "thread" } } });
  stream.end();
  await stream.done;
  await fake.waitFor(() => fake.threads.get("thread")?.subscribers.size === 0);
  connection.close();
  await connection.closed;
});

it("ends a stalled discovery, body read or upgrade with the caller's own reason, and classifies the rest as before", async () => {
  const connecting = async (fault: "holdDiscovery" | "holdDiscoveryBody" | "holdUpgrade") => {
    fake[fault] = true;
    const controller = new AbortController();
    const pending = connectT3({ url: fake.url, token: fake.token }, { signal: controller.signal });
    const failing = expect(pending).rejects.toThrow("core disconnected");
    await new Promise((resolve) => setTimeout(resolve, 100));
    const started = Date.now();
    controller.abort(new Error("core disconnected"));
    await failing;
    expect(Date.now() - started).toBeLessThan(500);
    fake[fault] = false;
    fake.disconnectAll();
  };
  await connecting("holdDiscovery");
  await connecting("holdDiscoveryBody");
  await connecting("holdUpgrade");
  // An already aborted caller sends nothing at all.
  const aborted = AbortSignal.abort(new Error("closed"));
  await expect(
    connectT3({ url: fake.url, token: fake.token }, { signal: aborted }),
  ).rejects.toThrow("closed");
  // Without an abort the failures keep their classification.
  const signal = new AbortController().signal;
  fake.protocolVersion = 1;
  await expect(connectT3({ url: fake.url, token: fake.token }, { signal })).rejects.toMatchObject({
    kind: "incompatible",
  });
  fake.protocolVersion = 2;
  await expect(
    connectT3({ url: "http://127.0.0.1:1", token: fake.token }, { signal }),
  ).rejects.toMatchObject({ kind: "unreachable" });
  await expect(connectT3({ url: fake.url, token: "other" }, { signal })).rejects.toMatchObject({
    kind: "refused",
  });
  const connection = await connectT3({ url: fake.url, token: fake.token }, { signal });
  connection.close();
  await connection.closed;
});
