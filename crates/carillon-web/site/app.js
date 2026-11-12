// A bare page that proves sign-in, reading and sending. The real page
// replaces these files. The routes are in ../README.md.

const status = document.querySelector("#status");
const form = document.querySelector("#send");
/** The conversation shown, and whether core has it yet. */
let open = { id: crypto.randomUUID(), started: false };

async function api(path, options = {}) {
  const response = await fetch(path, {
    ...options,
    headers: { authorization: `Bearer ${localStorage.getItem("token")}`, ...options.headers },
  });
  if (response.status === 204) return null;
  const body = await response.json();
  if (!response.ok) throw new Error(body.error);
  return body;
}

async function signIn() {
  const code = new URLSearchParams(location.hash.slice(1)).get("sign-in");
  if (code === null) return;
  history.replaceState(null, "", location.pathname);
  const { token } = await api("/api/sign-in", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ code }),
  });
  localStorage.setItem("token", token);
}

async function showConversations() {
  const { conversations } = await api("/api/conversations");
  const list = document.querySelector("#conversations");
  list.replaceChildren(
    ...conversations.map((conversation) => {
      const button = document.createElement("button");
      button.textContent = `${conversation.title || "Untitled"} (${conversation.agent_id})`;
      button.addEventListener("click", () => {
        open = { id: conversation.id, started: true };
        void showTurns();
      });
      const item = document.createElement("li");
      item.append(button);
      return item;
    }),
  );
}

async function showTurns() {
  const { turns } = await api(`/api/conversations/${open.id}/turns`);
  document.querySelector("#turns").replaceChildren(
    ...turns.map((turn) => {
      const item = document.createElement("li");
      const from = document.createElement("p");
      from.textContent = turn.from.kind === "person" ? turn.from.text : (turn.from.label ?? "");
      const outcome = document.createElement("div");
      // The provider rendered and sanitized this HTML.
      if (turn.html === undefined) outcome.textContent = turn.state;
      else outcome.innerHTML = turn.html;
      item.append(from, outcome);
      return item;
    }),
  );
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  const agent = open.started ? "" : `?agent=${encodeURIComponent(form.elements.agent.value)}`;
  // A form's own data would send an empty `file` part when none is chosen.
  const fields = new FormData();
  fields.set("text", form.elements.text.value);
  for (const file of form.elements.file.files) fields.append("file", file);
  try {
    await api(`/api/conversations/${open.id}/messages/m-${crypto.randomUUID()}${agent}`, {
      method: "PUT",
      body: fields,
    });
    open.started = true;
    form.reset();
    await Promise.all([showConversations(), showTurns()]);
  } catch (error) {
    status.textContent = String(error);
  }
});

try {
  await signIn();
  const { agents } = await api("/api/session");
  form.elements.agent.replaceChildren(...agents.map((agent) => new Option(agent.name, agent.id)));
  await showConversations();
  status.textContent = "Signed in.";
} catch {
  status.textContent = "Not signed in. Run `carillon web open`.";
}
