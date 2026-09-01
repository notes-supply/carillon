import { mkdtemp, readFile, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "vitest";

import { atomicJson } from "./atomic-json.ts";

const directories: string[] = [];

afterEach(async () => {
  await Promise.all(
    directories.splice(0).map((path) => rm(path, { recursive: true, force: true })),
  );
});

describe("atomicJson", () => {
  test("allows concurrent writers to publish complete files", async () => {
    const directory = await mkdtemp(join(tmpdir(), "carillon-atomic-json-"));
    directories.push(directory);
    const path = join(directory, "state.json");
    const values = Array.from({ length: 16 }, (_, value) => ({ value }));

    await Promise.all(values.map((value) => atomicJson(path, value)));

    const published: unknown = JSON.parse(await readFile(path, "utf8"));
    expect(values).toContainEqual(published);
    expect(await readdir(directory)).toEqual(["state.json"]);
  });
});
