import { expect, test } from "vitest";
import { size } from "./files.ts";

test("a file's size is written to two figures", () => {
  expect([0, 812, 3400, 20_417, 1_500_000, 1_073_741_824].map(size)).toStrictEqual([
    "0 B",
    "812 B",
    "3.4 kB",
    "20 kB",
    "1.5 MB",
    "1.1 GB",
  ]);
});
