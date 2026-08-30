import type { Api, Model } from "@earendil-works/pi-ai";
import { describe, expect, it } from "vitest";
import { profileEvidence, profileModel } from "./profile.ts";

const codexModel: Model<Api> = {
  id: "gpt-test",
  name: "GPT Test",
  api: "openai-codex-responses",
  provider: "openai-codex",
  baseUrl: "https://example.invalid",
  reasoning: true,
  input: ["text"],
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 128_000,
  maxTokens: 32_000,
};

const unsupportedModel: Model<"anthropic-messages"> = {
  id: "plain-test",
  name: "Plain Test",
  api: "anthropic-messages",
  provider: "anthropic",
  baseUrl: "https://example.invalid",
  reasoning: false,
  input: ["text"],
  cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
  contextWindow: 128_000,
  maxTokens: 32_000,
};

describe("execution profiles", () => {
  it("derives settings from Pi model capabilities", () => {
    const supported = profileModel(codexModel, "key", "OpenAI (ChatGPT Plus/Pro)");
    expect(supported.reasoning.length).toBeGreaterThan(0);
    expect(supported.provider_name).toBe("OpenAI (ChatGPT Plus/Pro)");
    expect(supported.service_tiers).toEqual([]);

    const unsupported = profileModel(unsupportedModel, "other", "Anthropic");
    expect(unsupported.reasoning).toEqual([]);
    expect(unsupported.service_tiers).toEqual([]);
  });

  it("does not invent provider-confirmed settings", () => {
    const evidence = profileEvidence(codexModel, "high");
    expect(evidence.effective.service_tier).toBeUndefined();
  });
});
