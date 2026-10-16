import { expect, it } from "vitest";
import { DETERMINISTIC_DEFAULT, describeSelection } from "./testing.ts";

it("names selections with their options", () => {
  expect(describeSelection(DETERMINISTIC_DEFAULT)).toBe("local/echo");
  expect(
    describeSelection({ provider: "local", model: "reasoner", options: { thinking: "high" } }),
  ).toBe("local/reasoner (thinking=high)");
});
