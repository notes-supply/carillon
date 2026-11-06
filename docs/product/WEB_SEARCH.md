# Add web search to a Pi agent

Carillon ships no web search. Search providers need an account key or a
server of your own, and the choice of provider is yours. A skill adds
search to a Pi agent. The skill is a `SKILL.md` file that tells the agent
how to call the provider's HTTP API with `curl` through its `bash` tool. It
needs no MCP server and no extension.

This page gives a complete skill for [Firecrawl](https://www.firecrawl.dev),
then shorter recipes for the Brave Search API and for SearXNG on your own
machine.

## Before you start

- The agent selects the `coding` extension, as it does by default. Pi lists
  skills in the prompt only with `coding` or `codemode`, and `coding` gives
  the agent `bash`.
- `curl` 7.82 or newer, which added the `--json` option.
- `jq`.
- A key from the provider, or a SearXNG server.

## Let the assistant do it

You can ask the assistant to set search up for you, for example:

> Set up Firecrawl web search for yourself, following
> https://github.com/notes-supply/carillon/blob/main/docs/product/WEB_SEARCH.md

It writes the skill. Store the key yourself, as the next section shows. Do
not send the key in a message. Carillon keeps messages in the conversation
history, and the model provider receives them.

## Store the key in a header file

Pi's `bash` tool runs `/bin/bash -c` in the environment of the
`carillon-runtime-pi` service. Its systemd unit sets that environment, so a
variable you export in your shell profile does not reach the agent. Keep
the key in a file that holds the whole HTTP header, and let `curl` read it
with `-H @file`.

Firecrawl keys start with `fc-`. Create one on the
[API keys page](https://www.firecrawl.dev/app/api-keys), then run this in
bash:

```sh
mkdir -p -m 700 ~/.config/firecrawl
read -rsp 'Firecrawl API key: ' key && echo
(umask 077 && printf 'Authorization: Bearer %s\n' "$key" > ~/.config/firecrawl/auth-header)
unset key
```

With `-H @file`, the command line names only the file, so activity cards
and the process list do not show the key. The agent runs as you and can
read the file, as it can read your other files.

Keep the file outside the agent's resource root, because core copies that
directory with every turn. The file is also outside `~/.carillon`, so
`carillon backup` does not include it.

## Add the Firecrawl skill

Create `skills/web-search/SKILL.md` in the agent's resource root. For the
assistant that setup creates, the file is
`~/.carillon/agents/default/skills/web-search/SKILL.md`. For another agent,
use the `resources` directory of its `[[agents]]` entry in `carillon.toml`.

````markdown
---
name: web-search
description: Search the web and read web pages. Use for news, prices, releases, documentation, and other facts that may have changed since training, and to read a URL someone sends.
---

# Web search

The commands read API keys from `auth-header` files under `~/.config`.
Never print, copy, or quote those files.

## Search

```sh
curl -sS https://api.firecrawl.dev/v2/search \
	-H @"$HOME/.config/firecrawl/auth-header" \
	--json "$(jq -n --arg q 'QUERY' '{query: $q, limit: 5}')" |
	jq 'if .success then .data.web[] | {title, url, description} else . end'
```

- Write the query as a person types it into a search engine.
- For recent results, add `tbs` to the body: `"qdr:d"` for the past day,
  `"qdr:w"` for the past week, or `"qdr:m"` for the past month.
- A result gives a title, a URL and a short description. Read the page
  before you rely on it.

## Read a page

```sh
curl -sS https://api.firecrawl.dev/v2/scrape \
	-H @"$HOME/.config/firecrawl/auth-header" \
	--json "$(jq -n --arg u 'URL' '{url: $u, formats: ["markdown"], onlyMainContent: true}')" |
	jq -r 'if .success then "status \(.data.metadata.statusCode)\n\(.data.markdown)" else . end'
```

- The status is the site's own HTTP status. A 403 or 404 means the page
  failed, even though Firecrawl answered.

## Answer

- Name the URLs you used.
- If the provider refuses the key or says the credits ran out, tell the
  person. Do not edit the key file.
````

The skill sends each search with `limit: 5`. Firecrawl's
[search reference](https://docs.firecrawl.dev/api-reference/endpoint/search)
and [scrape reference](https://docs.firecrawl.dev/api-reference/endpoint/scrape)
list the other fields, such as `location`, `includeDomains` and `maxAge`.
Firecrawl charges 2 credits for up to 10 search results and 1 credit for
each scraped page, as its [search guide](https://docs.firecrawl.dev/features/search)
says.

## Check that it works

The next message to the agent brings the skill. Nothing needs a restart.
Send a question the model cannot answer from training, for example:

> Search the web for the newest SearXNG release and link its release notes.

The activity card shows a `bash` call to
`https://api.firecrawl.dev/v2/search`, and the answer links its sources. If
the agent answers without searching, ask it to use its web search skill. An answer that says the key is wrong
means the header file does not hold `Authorization: Bearer fc-...`.

To remove search, delete the skill's directory.

## Use the Brave Search API instead

Get a key from the [Brave Search API dashboard](https://api-dashboard.search.brave.com).
Brave reads it from the `X-Subscription-Token` header:

```sh
mkdir -p -m 700 ~/.config/brave-search
read -rsp 'Brave Search API key: ' key && echo
(umask 077 && printf 'X-Subscription-Token: %s\n' "$key" > ~/.config/brave-search/auth-header)
unset key
```

Replace the command in the skill's Search section with this one:

```sh
curl -sS --get https://api.search.brave.com/res/v1/web/search \
	-H @"$HOME/.config/brave-search/auth-header" \
	-H 'Accept: application/json' \
	--data-urlencode 'q=QUERY' -d count=5 |
	jq 'if .web then .web.results[] | {title, url, description} else . end'
```

`count` takes up to 20. For recent results, add `-d freshness=pd` for the
past day, `pw` for the past week, or `pm` for the past month. Brave returns
links and snippets, not page text. Keep the Read a page section with its
Firecrawl key, or replace that section with an instruction to fetch pages
with `curl`. The
[web search guide](https://api-dashboard.search.brave.com/app/documentation/web-search/get-started)
lists the other parameters.

## Use your own SearXNG server instead

[SearXNG](https://docs.searxng.org) is a metasearch engine you run
yourself, with no key. It answers JSON only when its `settings.yml` lists
`json` under `search.formats`:

```yaml
search:
  formats:
    - html
    - json
```

Without `json` there, a JSON request gets 403 Forbidden. Replace the
command in the skill's Search section with this one, using your server's
address:

```sh
curl -sS --get http://127.0.0.1:8888/search \
	--data-urlencode 'q=QUERY' -d format=json |
	jq '.results[:5][] | {title, url, content}'
```

The [search API](https://docs.searxng.org/dev/search_api.html) also takes
`time_range` (`day`, `month` or `year`), `categories`, `language` and
`pageno`. Like Brave, SearXNG returns links and snippets, so read pages as
the Brave recipe says.

[Vane](https://github.com/ItzCrazyKns/Vane) is a self-hosted answer engine
built on SearXNG. It answers a question with its own model and sources at
`POST /api/search`, which its
[API guide](https://github.com/ItzCrazyKns/Vane/blob/master/docs/API/SEARCH.md)
describes.
