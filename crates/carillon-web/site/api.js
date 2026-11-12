// @ts-check
/**
 * Requests to the provider. Each carries the browser's token, and none
 * throws: an answer is its status, its body and the token the request
 * carried, and status 0 means nothing answered, which is how a stopped
 * provider looks.
 */

import { superseded } from "./model.js";

/** @typedef {{ status: number, body: string, token: string | null }} Answer */

const TOKEN = "token";

/** The browser's token, which every tab of this origin shares. */
export const token = {
  get: () => localStorage.getItem(TOKEN),
  /** @param {string} value */
  set: (value) => localStorage.setItem(TOKEN, value),
  drop: () => localStorage.removeItem(TOKEN),
};

/**
 * An answer to the token the browser has now. A 401 to a token replaced
 * while its request was on its way is asked again with the one stored.
 * @param {Answer} answer
 * @param {() => Promise<Answer>} again
 */
function current(answer, again) {
  return superseded(answer.status, answer.token, token.get()) ? again() : answer;
}

/**
 * @param {string} path
 * @param {"GET" | "POST"} [method]
 * @param {unknown} [json] The body of a POST.
 * @returns {Promise<Answer>}
 */
export async function ask(path, method = "GET", json) {
  const carried = token.get();
  try {
    const response = await fetch(path, {
      method,
      headers: {
        authorization: `Bearer ${carried ?? ""}`,
        ...(json === undefined ? {} : { "content-type": "application/json" }),
      },
      ...(json === undefined ? {} : { body: JSON.stringify(json) }),
    });
    const answer = { status: response.status, body: await response.text(), token: carried };
    return current(answer, () => ask(path, method, json));
  } catch {
    return { status: 0, body: "", token: carried };
  }
}

/**
 * Sends a message's form, reporting how much of it has left the browser.
 * `fetch` cannot say, so this is the page's one `XMLHttpRequest`.
 * @param {string} path
 * @param {FormData} form
 * @param {(sent: number, total: number, done: boolean) => void} progress
 * @returns {Promise<Answer>}
 */
export function put(path, form, progress) {
  const carried = token.get();
  return new Promise((resolve) => {
    const request = new XMLHttpRequest();
    request.open("PUT", path);
    request.setRequestHeader("authorization", `Bearer ${carried ?? ""}`);
    request.upload.onprogress = (event) => progress(event.loaded, event.total, false);
    request.upload.onload = (event) => progress(event.loaded, event.total, true);
    request.onload = () => {
      const answer = { status: request.status, body: request.responseText, token: carried };
      resolve(current(answer, () => put(path, form, progress)));
    };
    request.onerror = request.onabort = () => resolve({ status: 0, body: "", token: carried });
    request.send(form);
  });
}

/**
 * A 2xx answer's body as the type the provider's guide gives it. The
 * provider is the page's own origin and writes these shapes from its Rust
 * types, so the page takes them as given and checks nothing here.
 * @template T
 * @param {Answer} answer
 * @returns {T}
 */
export function data(answer) {
  // oxlint-disable-next-line typescript/no-unsafe-return -- the one place the provider's JSON gets its type
  return JSON.parse(answer.body);
}
