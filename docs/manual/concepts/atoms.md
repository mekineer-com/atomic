---
title: Atoms
description: Atoms are markdown notes with source metadata, tags, embeddings, and graph relationships.
---

Atoms are the fundamental unit of your Atomic knowledge base. Each atom is a markdown note that can be searched, tagged, linked, cited, and visualized.

## What Is an Atom?

An atom contains:

- **Content** - Markdown text.
- **Title** - Derived from the content or set manually.
- **Snippet** - A short preview for lists and search results.
- **Source URL** - Optional link to the original source.
- **Source** - Capture origin such as manual, imported, ingested URL, feed, or integration-created content.
- **Published date** - Optional timestamp for articles and imported material.
- **Tags** - Hierarchical labels, auto-extracted or manually assigned.
- **Pipeline status** - Embedding and tagging state for background processing.

## The Processing Pipeline

When you create or update an atom, Atomic can run an asynchronous pipeline:

1. **Chunking** - The content is split at markdown-aware boundaries such as headers, paragraphs, and code blocks.
2. **Embedding** - Each chunk is sent to the configured embedding provider.
3. **Tagging** - The full content is analyzed by an LLM to extract structured tags when auto-tagging is enabled.
4. **Edge building** - Vector similarity is computed against other atoms to find semantic relationships.

This pipeline is fire-and-forget from the caller's perspective. You get the saved atom immediately while processing continues in the background.

Each atom tracks `embedding_status` and `tagging_status`. Common values are `pending`, `processing`, `complete`, and `failed`; tagging can also be `skipped` when auto-tagging is disabled or not applicable.

## Creating Atoms

You can create atoms in several ways:

- **Editor** - Write directly in Atomic's markdown editor.
- **Web Clipper** - Use the [Atomic Web Clipper](/guides/browser-extension/) to save web pages or selected text.
- **API** - `POST /api/atoms` with markdown content.
- **Import** - Bulk import an Obsidian vault.
- **URL ingestion** - Ask Atomic to fetch and extract an article from a URL.
- **RSS/Atom feeds** - Subscribe to feeds and let Atomic create atoms for new entries.
- **MCP** - Let an MCP client create or update memory using Atomic tools.

In standalone mode, **New** creates a local atom and opens the editor. In
OpenAlma integrated mode, **New** asks for non-empty memory text and stores that
exact text as an approved memory in memU for the currently selected owner and
Soul; it does not create a local Atomic atom.

## Markdown Links

Atoms can contain wiki-style links using `[[...]]`. Atomic stores discovered links separately so the editor and graph views can resolve relationships between atoms while preserving unresolved targets for future editing.

## Duplicate Source URLs

URL ingestion and bulk capture paths can skip content when an atom with the same source URL already exists. This prevents feed polling and browser clipping from creating repeated copies of the same article.

## Related

- [Semantic Search](/concepts/semantic-search/)
- [Tags](/concepts/tags/)
- [URL Ingestion and Feeds](/guides/url-ingestion-and-feeds/)
- [API Overview](/api/overview/)
