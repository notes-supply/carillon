/** Authenticated HTTP adapter. Creating a server does not bind a listener. */
import { createHash, timingSafeEqual } from "node:crypto";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { Type } from "typebox";
import { Parse } from "typebox/value";
import {
  ModelQuerySchema,
  ModelUpdateSchema,
  type ModelQuery,
  type ModelUpdate,
} from "@carillon/protocol";
import { diagnostic, displayableExplanation, ExplainedFailure } from "./failure.ts";
import type { RuntimeOperations, TurnRequest } from "./operations.ts";
import packageInfo from "../package.json" with { type: "json" };

interface RuntimeServerOptions {
  readonly runtime: RuntimeOperations;
  readonly token: string;
  readonly providerId: string;
  readonly provider: "pi" | "deterministic-local-substitute";
  readonly workspace: string;
}
const ProviderIdentitySchema = Type.String({
  minLength: 1,
  maxLength: 128,
  pattern: "^[A-Za-z0-9_-]+$",
});
const TurnRequestSchema = Type.Object({
  correlation_id: Type.String(),
  interface_id: ProviderIdentitySchema,
  agent_id: Type.String(),
  audience_id: Type.String(),
  input: Type.String(),
  resource_root: Type.String(),
  native: Type.Optional(Type.Unknown()),
});

export function createRuntimeServer(options: RuntimeServerOptions) {
  if (options.token.length === 0) throw new Error("runtime credential must not be empty");
  return createServer((request, response) => {
    void route(request, response).catch((failure: unknown) => {
      // Callers receive only the displayable part of an explanation; the
      // diagnostic stays in this log. Refusing a caller's request (4xx) is
      // an answer, not a runtime failure.
      const explained = failure instanceof ExplainedFailure;
      if (!explained || failure.status >= 500) logFailure(request, failure);
      if (response.destroyed) return;
      const explanation = explained ? displayableExplanation(failure.message) : undefined;
      writeJson(
        response,
        explained ? failure.status : 500,
        explanation === undefined ? {} : { error: explanation },
      );
    });
  });
  async function route(request: IncomingMessage, response: ServerResponse): Promise<void> {
    if (request.method === "GET" && request.url === "/health") {
      writeJson(response, 200, {
        id: options.providerId,
        role: "runtime",
        version: packageInfo.version,
        status: "ready",
        features: ["native-continuations", "native-extensions", "skills"],
        provider: options.provider,
      });
      return;
    }
    if (!authorized(request)) {
      writeJson(response, 403, { error: "component authentication failed" });
      return;
    }
    if (request.method === "GET" && request.url === "/v1/environment") {
      const identity = unixIdentity();
      writeJson(response, 200, {
        ...identity,
        user: process.env["USER"],
        home: process.env["HOME"],
        shell: process.env["SHELL"],
        path: process.env["PATH"],
        workspace: options.workspace,
      });
      return;
    }

    if (request.method === "POST" && request.url === "/v1/turns") {
      let parsed: TurnRequest;
      try {
        parsed = Parse(TurnRequestSchema, await readJson(request));
      } catch {
        writeJson(response, 400, { error: "invalid runtime turn request" });
        return;
      }
      const controller = new AbortController();
      const disconnect = () => {
        if (!response.writableFinished) controller.abort();
      };
      response.on("close", disconnect);
      try {
        if (response.destroyed) controller.abort();
        const result = await options.runtime.executeTurn(parsed, controller.signal);
        writeJson(response, 200, result);
      } finally {
        response.off("close", disconnect);
      }
      return;
    }

    const url = new URL(request.url ?? "/", "http://localhost");
    if (request.method === "GET" && url.pathname === "/v1/model") {
      let query: ModelQuery;
      try {
        query = Parse(ModelQuerySchema, Object.fromEntries(url.searchParams));
      } catch {
        writeJson(response, 400, { error: "invalid model query" });
        return;
      }
      writeJson(response, 200, await options.runtime.modelState(query));
      return;
    }
    if (request.method === "PUT" && url.pathname === "/v1/model") {
      let update: ModelUpdate;
      try {
        update = Parse(ModelUpdateSchema, await readJson(request));
      } catch {
        writeJson(response, 400, { error: "invalid model update" });
        return;
      }
      // Not queued behind turns: a running turn resolved its selection when it
      // started, and the next turn resolves after this change is written.
      writeJson(response, 200, await options.runtime.setModel(update));
      return;
    }

    writeJson(response, 404, { error: "not found" });
  }

  function authorized(request: IncomingMessage): boolean {
    const candidate = request.headers.authorization?.replace(/^Bearer /u, "");
    if (candidate === undefined) return false;
    const expected = createHash("sha256").update(options.token).digest();
    const actual = createHash("sha256").update(candidate).digest();
    return timingSafeEqual(expected, actual);
  }

  async function readJson(request: IncomingMessage): Promise<unknown> {
    request.setEncoding("utf8");
    let body = "";
    for await (const chunk of request) {
      if (typeof chunk !== "string") throw new Error("request body is not UTF-8 text");
      body += chunk;
      if (Buffer.byteLength(body) > 1_048_576) throw new Error("request body exceeds 1 MiB");
    }
    return JSON.parse(body) as unknown;
  }

  /** Names the request by method and path: its query carries caller data. */
  function logFailure(request: IncomingMessage, failure: unknown): void {
    const path = (request.url ?? "").split("?", 1)[0];
    console.error(`${String(request.method)} ${String(path)} failed: ${diagnostic(failure)}`);
  }

  function writeJson(response: ServerResponse, status: number, value: unknown): void {
    const body = JSON.stringify(value);
    response.writeHead(status, {
      "content-type": "application/json",
      "content-length": Buffer.byteLength(body),
    });
    response.end(body);
  }

  function unixIdentity(): { readonly uid: number; readonly groups: number[] } {
    if (process.getuid === undefined || process.getgroups === undefined)
      throw new Error("Carillon Pi runtime requires Unix process identity APIs");
    return { uid: process.getuid(), groups: process.getgroups() };
  }
}
