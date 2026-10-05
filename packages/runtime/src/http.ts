/** Authenticated HTTP adapter. Creating a server does not bind a listener. */
import { createHash, timingSafeEqual } from "node:crypto";
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { Check, Parse } from "typebox/value";
import {
  ModelQuerySchema,
  ModelUpdateSchema,
  ProviderIdentitySchema,
  RuntimeTurnRequestSchema,
  type ModelQuery,
  type ModelUpdate,
} from "@carillon/protocol";
import { diagnostic, displayableExplanation, ExplainedFailure, RuntimeClosed } from "./failure.ts";
import type { RuntimeOperations, TurnRequest } from "./operations.ts";

interface RuntimeServerOptions {
  readonly runtime: RuntimeOperations;
  readonly token: string;
  readonly providerId: string;
  /** The engine behind this runtime, as `/health` names it. */
  readonly provider: string;
  /** The runtime's own build version, which core compares with the registry's expectation. */
  readonly version: string;
  readonly features: readonly string[];
  readonly workspace: string;
}

export function createRuntimeServer(options: RuntimeServerOptions) {
  if (options.token.length === 0) throw new Error("runtime credential must not be empty");
  return createServer((request, response) => {
    void route(request, response).catch((failure: unknown) => {
      // Callers receive only the displayable part of an explanation; the
      // diagnostic stays in this log. Refusing a caller's request (4xx) is
      // an answer, not a runtime failure.
      const explained = failure instanceof ExplainedFailure;
      if (response.destroyed) return;
      if (failure instanceof RuntimeClosed) {
        writeJson(response, 503, {});
        return;
      }
      if (!explained || failure.status >= 500) logFailure(request, failure);
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
        version: options.version,
        status: "ready",
        features: options.features,
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
        parsed = Parse(RuntimeTurnRequestSchema, await readJson(request));
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
    const activityPath = /^\/v1\/turns\/([^/]+)\/activity$/u.exec(url.pathname);
    if (request.method === "GET" && activityPath?.[1] !== undefined) {
      let correlationId: string;
      try {
        correlationId = decodeURIComponent(activityPath[1]);
      } catch {
        correlationId = "";
      }
      const interfaceId = url.searchParams.get("interface_id");
      if (correlationId.length === 0 || !Check(ProviderIdentitySchema, interfaceId)) {
        writeJson(response, 400, { error: "invalid activity query" });
        return;
      }
      // The interface must match the executing request, as a correlation
      // check: a turn is only ever described to the interface it serves.
      const activity = options.runtime.activity(correlationId, interfaceId);
      if (activity === undefined) writeJson(response, 404, { error: "turn is not executing" });
      else writeJson(response, 200, activity);
      return;
    }
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
      // The runtime decides how a change relates to the turns it runs.
      writeJson(response, 200, await options.runtime.setModel(update));
      return;
    }
    if (request.method === "GET" && url.pathname === "/v1/commands") {
      let query: ModelQuery;
      try {
        query = Parse(ModelQuerySchema, Object.fromEntries(url.searchParams));
      } catch {
        writeJson(response, 400, { error: "invalid command query" });
        return;
      }
      writeJson(response, 200, await options.runtime.commands(query));
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
      throw new Error("Carillon runtimes require Unix process identity APIs");
    return { uid: process.getuid(), groups: process.getgroups() };
  }
}
