import { SessionManager, SettingsManager } from "@earendil-works/pi-coding-agent";
import type { Api, Model, ModelsApiStreamOptions } from "@earendil-works/pi-ai";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import {
  latestExecutionProfile,
  loadExecutionProfileDefault,
  type PiServiceTierStreams,
  persistExecutionProfile,
  profileEvidence,
  profileModel,
  reasoningForModel,
  saveExecutionProfileDefault,
  streamSimpleForServiceTier,
} from "./profile.ts";

const codexModel: Model<Api> = {
  id: "gpt-test",
  name: "GPT Test",
  api: "openai-codex-responses",
  provider: "openai-codex",
  baseUrl: "https://example.invalid",
  reasoning: true,
  input: ["text"],
  cost: { input: 1, output: 2, cacheRead: 0.1, cacheWrite: 0 },
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

const context = { systemPrompt: "test", messages: [], tools: [] };

describe("execution profiles", () => {
  it("advertises speed only for Pi's Codex Responses models", () => {
    const supported = profileModel(codexModel, "key", "OpenAI (ChatGPT Plus/Pro)");
    expect(supported.reasoning.length).toBeGreaterThan(0);
    expect(supported.provider_name).toBe("OpenAI (ChatGPT Plus/Pro)");
    expect(supported.service_tiers).toEqual(["standard", "fast"]);

    const unsupported = profileModel(unsupportedModel, "other", "Anthropic");
    expect(unsupported.reasoning).toEqual([]);
    expect(unsupported.service_tiers).toEqual([]);
    expect(
      profileEvidence(unsupportedModel, undefined, "fast").requested.service_tier,
    ).toBeUndefined();
  });

  it("restores the applicable reasoning default for an existing conversation", async () => {
    const root = await mkdtemp(join(tmpdir(), "carillon-profile-"));
    try {
      const settings = SettingsManager.create(root, root);
      settings.setDefaultThinkingLevel("low");

      expect(reasoningForModel(settings, codexModel, "high")).toBe("high");
      expect(reasoningForModel(settings, codexModel, undefined)).toBe("low");
      expect(reasoningForModel(settings, unsupportedModel, undefined)).toBeUndefined();
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  it("uses Pi's native priority stream only for Fast and preserves its accounting", async () => {
    const priorityStream = {
      kind: "priority",
      result: () => Promise.resolve({ usage: { cost: { input: 2, output: 4 } } }),
    };
    const standardStream = { kind: "standard" };
    const nativeOptions: ModelsApiStreamOptions<"openai-codex-responses">[] = [];
    let standardCalls = 0;
    const runtime: PiServiceTierStreams<typeof priorityStream | typeof standardStream> = {
      stream: (_model, _context, options) => {
        nativeOptions.push(options ?? {});
        return priorityStream;
      },
      streamSimple: () => {
        standardCalls += 1;
        return standardStream;
      },
    };

    const fast = streamSimpleForServiceTier(runtime, "fast")(codexModel, context, {
      reasoning: "high",
    });
    expect(fast).toBe(priorityStream);
    expect(nativeOptions).toHaveLength(1);
    expect(nativeOptions[0]?.serviceTier).toBe("priority");
    expect(nativeOptions[0]?.reasoningEffort).toBe("high");
    expect(await priorityStream.result()).toEqual({
      usage: { cost: { input: 2, output: 4 } },
    });

    const standard = streamSimpleForServiceTier(runtime, "standard")(codexModel, context, {
      reasoning: "high",
    });
    expect(standard).toBe(standardStream);
    expect(nativeOptions).toHaveLength(1);
    expect(standardCalls).toBe(1);
  });

  it("creates and reuses an empty native continuation before its first turn", async () => {
    const root = await mkdtemp(join(tmpdir(), "carillon-profile-"));
    const sessions = join(root, "sessions");
    await mkdir(sessions);
    try {
      const first = SessionManager.create(root, sessions);
      const continuation = persistExecutionProfile(
        first,
        profileEvidence(codexModel, "high", "fast").requested,
      );
      expect(first.getEntries().every((entry) => entry.type !== "message")).toBe(true);

      const reopened = SessionManager.open(continuation);
      const reused = persistExecutionProfile(
        reopened,
        profileEvidence(codexModel, "low", "standard").requested,
      );
      expect(reused).toBe(continuation);
      expect(latestExecutionProfile(SessionManager.open(reused))).toMatchObject({
        reasoning: "low",
        service_tier: "standard",
      });
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  it("keeps the new-conversation default across restart without changing existing sessions", async () => {
    const root = await mkdtemp(join(tmpdir(), "carillon-profile-"));
    const sessions = join(root, "sessions");
    const defaultPath = join(root, "execution-profile.json");
    await mkdir(sessions);
    try {
      const existing = SessionManager.create(root, sessions);
      const existingFile = persistExecutionProfile(
        existing,
        profileEvidence(codexModel, "medium", "standard").requested,
      );
      const nextDefault = profileEvidence(codexModel, "high", "fast").requested;
      await saveExecutionProfileDefault(defaultPath, nextDefault, new AbortController().signal);

      const restoredDefault = await loadExecutionProfileDefault(defaultPath);
      expect(restoredDefault).toEqual(nextDefault);
      expect(latestExecutionProfile(SessionManager.open(existingFile))?.service_tier).toBe(
        "standard",
      );
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  it("does not save a new-conversation default after model resolution is cancelled", async () => {
    const root = await mkdtemp(join(tmpdir(), "carillon-profile-"));
    const defaultPath = join(root, "execution-profile.json");
    const controller = new AbortController();
    try {
      const selected = await Promise.resolve().then(() => {
        controller.abort();
        return profileEvidence(codexModel, "high", "fast").requested;
      });

      await expect(
        saveExecutionProfileDefault(defaultPath, selected, controller.signal),
      ).rejects.toMatchObject({ name: "AbortError" });
      await expect(loadExecutionProfileDefault(defaultPath)).resolves.toBeUndefined();
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });

  it("keeps simultaneous conversation tiers isolated across native continuations", async () => {
    const root = await mkdtemp(join(tmpdir(), "carillon-profile-"));
    const sessions = join(root, "sessions");
    await mkdir(sessions);
    try {
      const fast = SessionManager.create(root, sessions);
      const fastFile = persistExecutionProfile(
        fast,
        profileEvidence(codexModel, "high", "fast").requested,
      );
      const standard = SessionManager.create(root, sessions);
      const standardFile = persistExecutionProfile(
        standard,
        profileEvidence(codexModel, "low", "standard").requested,
      );

      const [restoredFast, restoredStandard] = await Promise.all([
        Promise.resolve(latestExecutionProfile(SessionManager.open(fastFile))),
        Promise.resolve(latestExecutionProfile(SessionManager.open(standardFile))),
      ]);
      expect(restoredFast).toMatchObject({ reasoning: "high", service_tier: "fast" });
      expect(restoredStandard).toMatchObject({ reasoning: "low", service_tier: "standard" });
      expect(profileEvidence(codexModel, "medium", "standard").requested).toMatchObject({
        reasoning: "medium",
        service_tier: "standard",
      });
    } finally {
      await rm(root, { recursive: true, force: true });
    }
  });
});
