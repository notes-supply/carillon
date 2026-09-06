import { expect, it } from "vitest";
import { runtimeListenAddress } from "./endpoint.ts";

it("converts supported loopback URLs to native listener addresses", () => {
  for (const [url, address] of [
    ["http://127.0.0.1:3000", { host: "127.0.0.1", port: 3000 }],
    ["http://127.2.3.4:3000", { host: "127.2.3.4", port: 3000 }],
    ["http://[::1]:3000", { host: "::1", port: 3000 }],
    ["http://127.0.0.1:80", { host: "127.0.0.1", port: 80 }],
    ["http://[::1]:80", { host: "::1", port: 80 }],
  ] as const)
    expect(runtimeListenAddress(url)).toEqual(address);
});

it("rejects unusable and non-loopback endpoints before Pi starts", () => {
  for (const url of [
    "not-a-url",
    "",
    "https://127.0.0.1:3000",
    "http://localhost:3000",
    "http://127.0.0.1",
    "http://127.0.0.1:99999",
    "http://127.0.0.1:3000/path",
    "http://user@127.0.0.1:3000",
    "http://127.0.0.1:3000?query",
    "http://127.0.0.1?query:3000",
    "http://127.0.0.1#fragment:3000",
    "http://0.0.0.0:3000",
    "http://192.168.1.2:3000",
    "http://[::]:3000",
    "http://[2001:db8::1]:3000",
    "http://127.0.0.1:0",
    "http://[::1]:0",
  ])
    expect(() => runtimeListenAddress(url), url).toThrow();
});
