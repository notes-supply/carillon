// @ts-check
/**
 * Requests to the provider. Each carries the browser's token, and none
 * throws: an answer is its status and body, and status 0 means nothing
 * answered, which is how a stopped provider looks.
 */

/** @typedef {{ status: number, body: string }} Answer */

const TOKEN = "token";

export const token = {
  /** @param {string} value */
  set: (value) => localStorage.setItem(TOKEN, value),
  drop: () => localStorage.removeItem(TOKEN),
};

const bearer = () => `Bearer ${localStorage.getItem(TOKEN) ?? ""}`;

/**
 * @param {string} path
 * @param {"GET" | "POST"} [method]
 * @param {unknown} [json] The body of a POST.
 * @returns {Promise<Answer>}
 */
export async function ask(path, method = "GET", json) {
  try {
    const response = await fetch(path, {
      method,
      headers: {
        authorization: bearer(),
        ...(json === undefined ? {} : { "content-type": "application/json" }),
      },
      ...(json === undefined ? {} : { body: JSON.stringify(json) }),
    });
    return { status: response.status, body: await response.text() };
  } catch {
    return { status: 0, body: "" };
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
  return new Promise((resolve) => {
    const request = new XMLHttpRequest();
    request.open("PUT", path);
    request.setRequestHeader("authorization", bearer());
    request.upload.onprogress = (event) => progress(event.loaded, event.total, false);
    request.upload.onload = (event) => progress(event.loaded, event.total, true);
    request.onload = () => resolve({ status: request.status, body: request.responseText });
    request.onerror = request.onabort = () => resolve({ status: 0, body: "" });
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
