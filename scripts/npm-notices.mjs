// Notices for the npm packages built into a release's JavaScript programs:
// each production dependency of the shipped workspace packages, as pnpm
// lists it, with the licence files it publishes. Printed to stdout.
// `node scripts/npm-notices.mjs RELEASE` checks that the staged RELEASE
// carries the standard text a package without a licence file refers to.
import { execFileSync } from "node:child_process";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

const SHIPPED = ["@carillon/runtime-pi", "@carillon/runtime-t3", "@carillon/discord-pi"];
// Standard texts in the release for packages that publish no licence file.
const STANDARD = { MIT: "licenses/MIT.txt", "Apache-2.0": "licenses/Apache-2.0.txt" };
const release = process.argv[2];
if (release === undefined) throw new Error("usage: node scripts/npm-notices.mjs RELEASE");
const LICENCE_FILE = /^(licen[cs]e|copying|notice)([.-]|$)/iu;

const listed = JSON.parse(
  execFileSync(
    "pnpm",
    ["licenses", "list", "--prod", "--json", ...SHIPPED.flatMap((name) => ["--filter", name])],
    { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
  ),
);
const packages = Object.values(listed)
  .flat()
  .filter((entry) => !entry.name.startsWith("@carillon/"))
  .flatMap((entry) =>
    entry.versions.map((version, index) => ({ ...entry, version, path: entry.paths[index] })),
  )
  .sort((a, b) => a.name.localeCompare(b.name) || a.version.localeCompare(b.version));
if (packages.length === 0) throw new Error("pnpm listed no production dependencies");

const sections = packages.map(({ name, version, license, author, homepage, path }) => {
  const files = readdirSync(path)
    .filter((file) => LICENCE_FILE.test(file))
    .sort();
  let body;
  if (files.length > 0) {
    body = files.map((file) => readFileSync(join(path, file), "utf8").trimEnd()).join("\n\n");
  } else {
    const standard = STANDARD[license];
    if (standard === undefined || !existsSync(join(release, standard)))
      throw new Error(`${name}@${version} publishes no licence file and declares ${license}`);
    body = `This package publishes no licence file. Its package.json declares ${license}${author ? `, by ${author}` : ""}; the standard text is ${standard} in this release.`;
  }
  const heading = `${name} ${version} (${license})${homepage ? `\n${homepage}` : ""}`;
  return `${"=".repeat(80)}\n${heading}\n\n${body}\n`;
});
process.stdout.write(
  `npm packages built into carillon-runtime-pi, carillon-runtime-t3 and\ncarillon-discord-pi.js, with their licences. Generated from pnpm's listing\nof their production dependencies.\n\n${sections.join("\n")}`,
);
