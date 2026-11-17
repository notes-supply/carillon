import { parseArgs } from "node:util";
import { scenarios } from "./scenarios.ts";
import { serve } from "./server.ts";

const { values } = parseArgs({
  options: {
    port: { type: "string" },
    site: { type: "string" },
    scenario: { type: "string", default: "rail" },
    clock: { type: "string", default: "live" },
  },
});

const port = Number(values.port);
if (
  !Number.isInteger(port) ||
  scenarios[values.scenario] === undefined ||
  (values.clock !== "manual" && values.clock !== "live")
) {
  console.error(
    [
      "usage: node fake/main.ts --port <port, or 0 for any> [--site <folder of the built page>]",
      "       [--scenario <name>] [--clock live|manual]",
      "",
      ...Object.entries(scenarios).map(([name, scenario]) => `  ${name}: ${scenario.about}`),
    ].join("\n"),
  );
  process.exit(2);
}

const fake = await serve({
  port,
  site: values.site,
  scenario: values.scenario,
  clock: values.clock,
});
console.log(JSON.stringify({ port: fake.port, scenario: values.scenario, clock: values.clock }));

for (const signal of ["SIGINT", "SIGTERM"] as const)
  process.on(signal, () => void fake.close().then(() => process.exit(0)));
