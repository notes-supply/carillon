import { readFileSync, readdirSync } from "node:fs";
import { extname, join } from "node:path";
import { expect, test } from "./fixtures.ts";

const SERVED = [".html", ".js", ".css", ".svg", ".png", ".woff2", ".txt"];

test("the page is one flat folder of files the provider serves", ({ site }) => {
  const entries = readdirSync(site, { withFileTypes: true });
  expect(entries.filter((entry) => !entry.isFile()).map((entry) => entry.name)).toEqual([]);
  expect(
    entries.map((entry) => entry.name).filter((name) => !SERVED.includes(extname(name))),
  ).toEqual([]);
  expect(entries.map((entry) => entry.name)).toContain("index.html");
});

test("nothing in the page needs a looser policy than the provider sets", ({ site }) => {
  const html = readFileSync(join(site, "index.html"), "utf8");
  expect(html).not.toMatch(/<script(?![^>]*\ssrc=)[^>]*>/iu);
  expect(html).not.toMatch(/<style[\s>]/iu);
  expect(html).not.toMatch(/\sstyle\s*=/iu);
  expect(html).not.toMatch(/\son[a-z]+\s*=/iu);
  expect(html).not.toMatch(/(?:src|href)\s*=\s*["']?(?:https?:)?\/\//iu);
  for (const name of readdirSync(site).filter((each) => extname(each) === ".js")) {
    const script = readFileSync(join(site, name), "utf8");
    expect(script, name).not.toMatch(/\beval\s*\(|\bnew\s+Function\s*\(|\bglobalThis\.Function\b/u);
  }
});
