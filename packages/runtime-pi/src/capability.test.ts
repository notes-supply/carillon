import { afterEach, expect, it, vi } from "vitest";
import { callCapability, type CapabilitySelection } from "./capability.ts";

const selection: CapabilitySelection = {
  provider: {
    id: "echo-capability",
    role: "capability",
    version: "1",
    url: "http://127.0.0.1:1",
  },
  token: "token",
};

afterEach(() => {
  vi.unstubAllGlobals();
});

it("retries an ambiguous response with the same correlation", async () => {
  const fetch = vi
    .fn<typeof globalThis.fetch>()
    .mockResolvedValueOnce(new Response("not JSON"))
    .mockResolvedValueOnce(
      Response.json({ output: "external echo: hello", native: { sequence: 1 } }),
    );
  vi.stubGlobal("fetch", fetch);

  await expect(callCapability(selection, "turn:call", "hello")).resolves.toEqual({
    output: "external echo: hello",
    native: { sequence: 1 },
  });
  expect(fetch).toHaveBeenCalledTimes(2);
  expect(fetch.mock.calls[0]?.[1]?.body).toBe(fetch.mock.calls[1]?.[1]?.body);
});

it("does not retry an explicit capability rejection", async () => {
  const fetch = vi
    .fn<typeof globalThis.fetch>()
    .mockResolvedValue(new Response(null, { status: 401 }));
  vi.stubGlobal("fetch", fetch);

  await expect(callCapability(selection, "turn:call", "hello")).rejects.toThrow(
    "capability echo-capability returned HTTP 401",
  );
  expect(fetch).toHaveBeenCalledOnce();
});
