import { Type } from "typebox";
import { Parse } from "typebox/value";

export interface CapabilitySelection {
  readonly provider: {
    readonly id: string;
    readonly role: string;
    readonly version: string;
    readonly url: string;
  };
  readonly token: string;
}

export interface CapabilityResponse {
  readonly output: string;
  readonly native: unknown;
}

const CapabilityResponseSchema = Type.Object({
  output: Type.String(),
  native: Type.Unknown(),
});
const REQUEST_TIMEOUT_MS = 15_000;

class CapabilityHttpError extends Error {}

async function attempt(
  selection: CapabilitySelection,
  correlationId: string,
  input: string,
): Promise<CapabilityResponse> {
  const response = await fetch(`${selection.provider.url}/v1/execute`, {
    method: "POST",
    headers: { authorization: `Bearer ${selection.token}`, "content-type": "application/json" },
    body: JSON.stringify({ correlation_id: correlationId, operation: "echo", input }),
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
  });
  if (!response.ok)
    throw new CapabilityHttpError(
      `capability ${selection.provider.id} returned HTTP ${String(response.status)}`,
    );
  return Parse(CapabilityResponseSchema, await response.json());
}

export async function callCapability(
  selection: CapabilitySelection,
  correlationId: string,
  input: string,
): Promise<CapabilityResponse> {
  try {
    return await attempt(selection, correlationId, input);
  } catch (failure: unknown) {
    if (failure instanceof CapabilityHttpError) throw failure;
    return attempt(selection, correlationId, input);
  }
}
