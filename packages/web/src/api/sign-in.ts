import { ORIGIN } from "./origin.ts";
import {
  SIGN_IN_PATH,
  SignedInSchema,
  UNREADABLE,
  readAnswer,
  reading,
  refusalWords,
  signInBody,
} from "./requests.ts";

export type SignIn = { kind: "signed_in"; token: string } | { kind: "refused"; words: string };

export async function requestSignIn(password: string): Promise<SignIn> {
  const answer = await fetch(ORIGIN + SIGN_IN_PATH, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: signInBody(password),
  })
    .then(async (response) => ({ status: response.status, body: await response.text() }))
    .catch(() => ({ status: 0, body: "" }));
  if (reading(answer.status) !== "ok")
    return { kind: "refused", words: refusalWords(answer.status, answer.body) };
  const signedIn = readAnswer(SignedInSchema, answer.body);
  return signedIn === undefined
    ? { kind: "refused", words: UNREADABLE }
    : { kind: "signed_in", token: signedIn.token };
}
