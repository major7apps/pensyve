# Documentation and search audit, October 4, 2026

Pensyve's documentation needed clearer setup paths and corrections to several
API descriptions after the Cloud service closed. Search data supports describing
the project as persistent AI agent memory, with separate routes for Python,
MCP clients, and a gateway that users host themselves.

This audit measures the public baseline and records documentation changes.
The measurements were collected before publication, so ranking or traffic
improvement has not been measured. The source baseline is commit
`25b3b0204c030e5be7137a73ccec2e97b9a35c3f`.

## Evidence and method

The [measurement snapshot](2026-10-04-search-baseline.json) preserves request
parameters, returned keyword metrics, search result URLs, page metadata, and
GitHub traffic. Missing keyword records and null volumes remain distinct from
zero. It also includes the earlier exploratory pass and hashes of the expanded
pass's raw API responses.

| Measurement | Coverage |
| --- | --- |
| Keyword overview | 30 requested phrases, 22 returned records, 21 with a volume estimate |
| Keyword suggestions | 40 results for each of three seeds, giving 120 records and 118 distinct phrases |
| Combined expanded keyword data | 133 distinct phrases with returned records |
| Google search results | Nine queries and 231 returned organic results |
| Public page checks | GitHub repository, GitHub documentation directory, and PyPI project |
| GitHub baseline | Repository metadata, page views, referral sources, and popular paths |

Keyword and search-result calls used US location code `2840` and English
language code `en`.
Search results used desktop queries. Eight queries requested depth 30, and the
repository `site:` query requested depth 10. The returned organic counts were
smaller than those depths, so the counts below use actual records. All 16
expanded API requests and their tasks returned status `20000`.

Keyword figures are DataForSEO estimates from its Google Ads based database.
They are not Pensyve visits or a forecast of available traffic. Most returned
metrics were updated in September 2026 and had August 2026 as their latest
monthly record. The record for `python persistent memory` was older and had no
aggregate volume. See the [keyword metric definitions](https://docs.dataforseo.com/v3/dataforseo_labs-google-keyword_overview-live/).

The expanded calls cost $0.11549. The earlier exploratory calls cost $0.02120,
for a reported API total of $0.13669. Requests were cached locally to avoid
paying again during analysis.

## Search demand and documentation decisions

The following phrases helped identify useful entry points. Similar phrases
may overlap, so their volumes are not added into a market total. Broad client
terms also include searches for the client's built-in memory features.

| Search phrase | Estimated monthly US searches | Documentation decision |
| --- | ---: | --- |
| agent memory | 1,300 | Explain what the store does and how an agent uses it |
| ai agent memory | 210 | Use the concrete category in the opening paragraph |
| llm memory | 260 | Explain that saved memory is separate from model weights |
| long term memory ai | 320 | Explain persistence across processes and sessions |
| memory mcp server | 140 | Provide a working stdio setup and the full tool reference |
| mcp memory server | 70 | Use the same MCP guide, without a duplicate page |
| claude code memory | 1,300 | Link directly to the plugin guide and explain tool-based capture |
| claude memory mcp | 110 | Make local MCP and hosted gateway setup distinct |
| claude code persistent memory | 70 | Explain the storage and session behavior |
| cursor memory | 140 | Add a direct Cursor setup link |
| cursor memory mcp | 10 | Verify the Cursor configuration and rules |
| langchain memory | 260 | Link to the adapter and state its supported interface |
| langgraph memory | 210 | Correct unsupported drop-in store claims |
| ai agent memory types | 10 | Explain the four stored memory types |
| local ai memory | 10 | Qualify offline use with model preparation instructions |

The overview returned no records for `agent memory vs rag`, `llm memory vs rag`,
`open source ai memory`, `pensyve`, `pensyve github`,
`persistent memory for ai agents`, `python agent memory`, or
`self hosted ai memory`. That does not establish zero demand. Local operation,
open-source status, and Python setup remain important descriptions of the
actual project.

Suggestion results also contained named competing products, research titles,
hardware memory calculators, and unrelated entertainment searches. These were
retained in the snapshot but did not become target pages. For example, a
training-memory calculator does not describe this runtime. Search volume alone
was not used to choose documentation topics.

The returned questions included how to give an agent memory and how memory is
stored. These support a short explanation of saving, retrieving, and adding
records to a prompt. They do not justify claims that Pensyve trains a model or
learns procedures automatically.

## Search visibility baseline

These results were retrieved on October 4, 2026, between 12:25:00 and 12:25:26
UTC. The snapshot keeps the ordered organic URLs and both organic and absolute
positions for each result.

| Query | Organic results returned | Pensyve result |
| --- | ---: | --- |
| ai agent memory | 29 | None |
| persistent memory for ai agents | 29 | None |
| mcp memory server | 29 | None |
| claude code memory | 29 | None |
| cursor memory mcp | 29 | None |
| python agent memory | 28 | None |
| self hosted ai memory | 28 | None |
| pensyve github | 29 | None |
| site:github.com/major7apps/pensyve | 1 | `tests/python` directory, organic position 1 |

The repository root was absent from these returned results. The `site:` result
shows that a repository subdirectory surfaced, so the audit does not establish
that the repository is unindexed. It also does not establish an exact position
outside the returned results. The earlier broad brand queries produced spelling
suggestions, which limits conclusions from those samples.

## Public pages and repository traffic

All three page checks returned HTTP 200. The public repository still showed the
old README, including the competitive comparison section. The documentation
directory had no README introduction. PyPI still showed the old description and
README, without the current project-status section.

GitHub's repository description was `Universal memory runtime for AI agents`,
and its homepage field was empty. The PyPI description used broad claims such
as `offline-first`, without explaining the initial model downloads. Source
manifest descriptions now say what each component does, but those edits do not
update GitHub settings or an already published package.

The page checks include GitHub and PyPI navigation and page templates. Their
word counts, readability scores, title warnings, and content ratios are not
used as README quality scores. Template flags do not establish defects that
can be fixed by editing this repository.

GitHub's returned daily view records covered September 20 through October 3,
2026. Its API reports rolling 14-day traffic and the most popular referral
sources and paths. See [GitHub's traffic definitions](https://docs.github.com/en/rest/metrics/traffic?apiVersion=2022-11-28).

| GitHub measure | Views | Unique visitors |
| --- | ---: | ---: |
| Repository total | 347 | 101 |
| Overview page | 107 | 52 |
| Codex plugin path | 20 | 19 |
| OpenCode plugin path | 16 | 13 |
| Cursor path | 9 | 9 |
| Google referral source | 7 | 6 |
| DuckDuckGo referral source | 1 | 1 |

These counts are a baseline, not keyword-attributed conversions. Unique visitors
can overlap across paths and referral sources, so those rows must not be added.
The snapshot retains every returned popular path and referral source, including
pull requests and other sources omitted from this display.

## Local improvements

The checks below compare the baseline commit with the edited files. They assess
specific documentation tasks, not a combined SEO score.

| Check | Baseline | Edited documentation |
| --- | --- | --- |
| Opening describes persistent memory and concrete stored content | Broad runtime description | Explicit storage, retrieval, and client responsibilities |
| Documentation directory has an entry page | Missing | Task-based index with setup and reference links |
| Local persistence is explained | Incomplete path and reopen guidance | Explicit path, namespace, and later-session reuse |
| MCP install command matches configuration | Installed binary was referenced through a build-directory path | `pensyve-mcp` is installed and configured consistently |
| Gateway quick start demonstrates authentication | Incomplete setup | Loopback gateway and matching `psy_` example key |
| README and getting-started guide link directly to Cursor | Missing | Present in both |
| Offline use explains model preparation | Incomplete guidance and broken container script layout | Model preparation and matching container paths |
| Memory is distinguished from model training | Missing explanation | Saving and retrieval are explained separately from model weights |
| Historical plans are distinguished from current setup guides | No documentation index | Historical section in the index |
| Integration transport requirements are clear | Python and HTTP paths were conflated | Local Python, MCP, and gateway clients are distinguished |

The [README](../../README.md) now routes readers to Python, MCP, HTTP, and
framework setup. The [documentation index](../README.md) links directly to the
clients and frameworks supported by the repository. The [LLM documentation
index](../../llms.txt) points to the same public guides.

The accuracy review corrected 24 targeted stale claims across
[architecture](../ARCHITECTURE.md), [recipes](../RECIPES.md), and the
[MCP reference](../../pensyve-mcp/README.md). These included the retrieval formula,
automatic procedural learning, consolidation and retained rows, model startup,
and deletion snapshots. The MCP reference increased from seven documented
tools to all ten runtime tools. The check count is a review checklist, not an
exhaustive count of defects or proof of runtime correctness.

SDK and integration guides now explain the relevant limitations. The TypeScript
and Go episode helpers do not match the current gateway routes. TypeScript's
inspect response mapping and configured namespace also have limits. The guides
provide direct REST alternatives where appropriate. The LangChain guides state
the adapter's actual methods and deletion scope. These are existing runtime
limitations; this change updates documentation rather than changing those APIs.

Nine component descriptions were updated in TOML and JSON manifests. Versions,
dependencies, classifiers, and package URLs were preserved. Current setup links
that pointed to the separate `pensyve-docs` repository were replaced with public
files here. Historical research tools retain their references and are identified
in the maintenance policy.

## Verification and remaining measurements

Local verification checks document links and heading anchors, fenced Python,
shell, and JSON syntax, package manifest parsing, and Cargo binary names.
TypeScript SDK examples are type-checked, and Go SDK examples are compiled.
The final pass covered 33 documentation files and 243 local links or anchors.
Syntax checks passed for 21 Python, 88 shell, and 51 JSON snippets. All nine
description-only manifests parsed, and three documented Cargo binary names
matched workspace metadata. Two SDK TypeScript examples passed strict type
checking, and three Go examples compiled offline. Two integration TypeScript
snippets passed syntax checks. Whitespace checks passed.

The initial example checks did not run a current gateway or prove retrieval
behavior. At that stage, the installed Python native extension was older than
the source, so it was not used to claim execution of the Python examples.
Integration snippet syntax checks do not establish compatibility with installed
framework versions.

During PR preparation, `make check` rebuilt the current Python extension and
passed Rust and Python lint, builds, and tests. The results were 1,390 Rust tests
passed with 10 ignored, and 127 Python tests passed. Separate SDK checks passed
TypeScript lint, build, and 101 tests, plus Go vet and 44 tests with 10 subtests.
These results validate the tested code paths; they do not measure search rankings
or installation success for new users.

The repository description can be updated to
`Open-source persistent memory for AI agents, with Python, MCP, and a self-hosted REST API.`
After the documentation is published, the homepage field can point to
`https://github.com/major7apps/pensyve/tree/main/docs`.
These remote settings and package publications are unchanged by this local pass.

First, publish the reviewed documentation through the normal repository process
and confirm that the public pages show it. A later package release must carry
the updated package README and description if PyPI is to change as well.

Second, repeat the nine saved search requests with the same location, language,
device, and depths. Record the actual result counts and spelling changes, and
compare the exact repository and documentation URLs. A check after one week and
another after four weeks would provide observations without assuming a crawl
schedule or a guaranteed ranking change.

Third, capture GitHub traffic weekly so its rolling window is not lost. Compare
non-overlapping periods for views, Google referrals, and documentation paths.
Changes in releases, links from other sites, and general demand can affect those
counts, so a before-and-after change alone does not establish causation.

Ranking gains, search impressions, search click-through rate, and successful
installation or recall by new users remain unmeasured. No follow-up monitor or
publication was created by this audit.
