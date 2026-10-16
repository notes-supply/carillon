/** A catalogue shaped like the serving build's: two usable instances and one disabled. */
export const FAKE_PROVIDERS: readonly unknown[] = [
  {
    instanceId: "claudeAgent",
    driver: "claudeAgent",
    enabled: true,
    installed: true,
    status: "ready",
    models: [
      {
        slug: "claude-sonnet-5",
        name: "Claude Sonnet 5",
        isCustom: false,
        capabilities: {
          optionDescriptors: [
            {
              id: "effort",
              label: "Effort",
              type: "select",
              options: [
                { id: "low", label: "Low" },
                { id: "high", label: "High", isDefault: true },
              ],
            },
            { id: "fastMode", label: "Fast Mode", type: "boolean" },
          ],
        },
      },
      { slug: "claude-haiku-4-5", name: "Claude Haiku 4.5", isCustom: false, capabilities: null },
    ],
  },
  {
    instanceId: "codex",
    driver: "codex",
    enabled: true,
    installed: true,
    status: "ready",
    models: [{ slug: "gpt-6-astra", name: "GPT-6 Astra", isCustom: false, capabilities: null }],
  },
  {
    instanceId: "cursor",
    driver: "cursor",
    enabled: false,
    installed: false,
    status: "disabled",
    models: [{ slug: "auto", name: "Auto", isCustom: false, capabilities: null }],
  },
];
