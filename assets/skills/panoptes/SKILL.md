---
name: panoptes
description: Use Panoptes MCP for coding tasks that require locating, understanding, searching, tracing, or mapping indexed source. Prefer it before built-in grep, search, or whole-file reads; use find for ranked code context, grep for exhaustive occurrences, callers for dependency and blast-radius tracing, skeleton for a file API, and map for repository orientation.
---

# Panoptes

<!-- panoptes:managed-skill -->

Use Panoptes first when the task depends on indexed source. One focused call can
replace several searches and whole-file reads while still returning exact paths,
line spans, signatures, and bounded source.

## Choose the tool

- Use `find` for questions such as “where is this implemented?” or “how does this
  work?” Source excerpts are included by default. Read the exact reported span
  only when the excerpt is insufficient or truncated.
- Use `grep` when every textual occurrence matters. Set `fixed` for literal text,
  and use `in` to limit the search to a path.
- Use `callers` to follow incoming or outgoing dependencies and assess blast
  radius. Increase `depth` only when the first hop is insufficient.
- Use `skeleton` to inspect a file's definitions, signatures, and spans without
  reading its implementation.
- Use `map` to orient in an unfamiliar repository before choosing a narrower
  query.
- Use `status` or `freshness` only to inspect index state. Normal retrieval calls
  create or refresh the index automatically unless refresh is disabled.

## Work from the result

- Act from returned paths and spans rather than repeating the same query with
  slightly different wording. Switch tools when the question changes from
  relevance to exhaustiveness or graph traversal.
- Fall back to built-in search or targeted reads when Panoptes has no suitable
  result, the file type is not indexed, or exact surrounding implementation is
  required.
- Treat the `panoptesSavings` values as estimates based on four characters per
  token versus reading the matched source files whole. When Panoptes was used,
  report the MCP session's estimated tokens saved in one concise final line.
  Never present that estimate as model billing or an externally measured token
  count.
