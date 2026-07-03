# Atomic — Feature Inventory

Reference catalog of every user-facing feature/capability. Anchors are `file:line` for backend routes (`crates/atomic-server/src/`), core functions (`crates/atomic-core/src/`), and frontend components (`src/`).

Legend for backend needs: R=read, W=write, EMB=embedding, LLM=LLM call, BG=background job, WS=WebSocket stream.

---

## (a) Core Graph / Edit Features

### 1. Canvas / Global Graph View
- Force-directed graph of all atoms (Sigma.js + Graphology); nodes=atoms, edges=semantic similarity + shared tags.
- Needs: R.
- Data: atoms, semantic_edges, atom_positions, tags.
- Anchors: `src/components/canvas/SigmaCanvas.tsx:49`; route `crates/atomic-server/src/routes/canvas.rs:28 get_atoms_with_embeddings`, `canvas.rs:73 get_global_canvas`.

### 2. Canvas Level Hierarchy
- Hierarchical canvas: root shows tag clusters, drill-in shows atoms; oversized groups auto-clustered by similarity.
- Needs: R.
- Data: tags, atoms, semantic_edges.
- Anchors: `src/components/canvas/SigmaCanvas.tsx:49`; route `canvas.rs:50 get_canvas_level`; core `crates/atomic-core/src/canvas_level.rs:152 get_canvas_level`, `canvas_level.rs:264 build_tag_level`, `canvas_level.rs:616 cluster_tags_by_similarity`.

### 3. Semantic-Similarity Layout ("similar things attract")
- Semantic edges weighted by embedding cosine similarity (threshold >=0.5) plus PCA-projected 2D seed positions determine visual proximity.
- Needs: R, EMB (upstream).
- Data: vec_chunks embeddings, semantic_edges (similarity_score), atom_positions.
- Anchors: core `crates/atomic-core/src/projection.rs:11 compute_2d_projection`, `canvas_level.rs:1096 load_semantic_edges_for_atoms`; frontend `src/components/canvas/SigmaCanvas.tsx:229`; mini d3-force `src/components/canvas/useMiniForceSimulation.ts:39`.

### 4. Local Neighborhood View
- Shows only the N-hop neighbor subgraph of a selected atom with hover highlighting.
- Needs: R.
- Data: semantic_edges, atoms.
- Anchors: `src/components/canvas/LocalGraphView.tsx:217`; route `crates/atomic-server/src/routes/graph.rs:32 get_atom_neighborhood`.

### 5. Atom CRUD + Markdown Editor
- Create/read/update/delete atoms; CodeMirror 6 markdown editing with preview.
- Needs: R, W (+ EMB/LLM triggered on save).
- Data: atoms.
- Anchors: `src/components/atoms/AtomReader.tsx:49`, editor `src/editor/`; routes `crates/atomic-server/src/routes/atoms.rs:332 create_atom`, `atoms.rs:218 get_atom`, `atoms.rs:427 update_atom`, `atoms.rs:530 delete_atom`.

### 6. Atom View Modes (grid / list)
- Toggle between card grid and list layout for the atoms panel.
- Needs: R.
- Data: atoms.
- Anchors: `src/stores/ui.ts:7` (AtomsLayout); `src/components/layout/MainView.tsx:37`; grid `src/components/atoms/AtomGrid.tsx`, list `src/components/atoms/AtomList.tsx`.

### 7. Node Position Persistence
- Saves/restores dragged atom x/y positions across sessions.
- Needs: R, W.
- Data: atom_positions.
- Anchors: routes `crates/atomic-server/src/routes/canvas.rs:14 get_positions`, `canvas.rs:19 save_positions`; frontend `src/components/canvas/SigmaCanvas.tsx:49`.

### 8. Manual Atom Links (wiki-links)
- `[[title]]` references between atoms, parsed on save; editor autocomplete.
- Needs: R, W.
- Data: atom_links.
- Anchors: core `crates/atomic-core/src/atom_links.rs:25 extract_atom_link_tokens`; routes `atoms.rs:239 get_atom_links`, `atoms.rs:263 get_atom_link_suggestions`; frontend `src/editor/atom-links/`.

### 9. Related Atoms Panel
- Lists semantically similar atoms for the open atom, ranked by embedding distance.
- Needs: R.
- Data: vec_chunks, atoms.
- Anchors: `src/components/atoms/RelatedAtoms.tsx:24`; route `crates/atomic-server/src/routes/search.rs:111 find_similar`; core `crates/atomic-core/src/search.rs:823 find_similar_atoms`.

### 10. Tag Tree / Hierarchical Tag Panel
- Sidebar tag hierarchy (parent→child); selecting a tag filters views.
- Needs: R.
- Data: tags.
- Anchors: `src/components/tags/TagTree.tsx:52`; routes `atoms.rs:564 get_tags`, `atoms.rs:581 get_tag_children`; store `src/stores/tags.ts`.

### 11. Tag CRUD
- Create/rename/delete tags; mark auto-tag targets; set per-tag LLM description.
- Needs: R, W.
- Data: tags.
- Anchors: routes `atoms.rs:614 create_tag`, `atoms.rs:643 update_tag`, `atoms.rs:669 delete_tag`, `atoms.rs:704 set_tag_autotag_target`, `atoms.rs:730 set_tag_autotag_description`, `atoms.rs:762 configure_autotag_targets`; store `src/stores/tags.ts:223`.

### 12. Semantic Edge Rebuild
- Recompute all similarity edges from current embeddings.
- Needs: R, W, BG.
- Data: vec_chunks -> semantic_edges.
- Anchors: route `crates/atomic-server/src/routes/graph.rs:54 rebuild_semantic_edges`; core `crates/atomic-core/src/graph_maintenance.rs:118 run_now`, `embedding.rs:2159 compute_semantic_edges_for_atom`.

### 13. Atom Clustering
- Groups atoms into semantic clusters; used for canvas level-of-detail.
- Needs: R.
- Data: atoms, semantic_edges.
- Anchors: routes `crates/atomic-server/src/routes/clustering.rs:18 compute_clusters`, `clustering.rs:36 get_clusters`, `clustering.rs:48 get_connection_counts`; core `crates/atomic-core/src/clustering.rs`.

### 14. Export (Markdown)
- Exports a database's atoms as a markdown zip via async background job.
- Needs: R, BG.
- Data: atoms.
- Anchors: routes `crates/atomic-server/src/routes/exports.rs:13 start_markdown_export`, `exports.rs:28 get_export_job`, `exports.rs:68 download_export`; core `crates/atomic-core/src/export.rs`.

### 15. Obsidian Vault Import
- Bulk-imports a directory of Obsidian markdown files as atoms.
- Needs: W, EMB, LLM (pipeline).
- Data: atoms.
- Anchors: route `crates/atomic-server/src/routes/import.rs:19 import_obsidian_vault`; core `crates/atomic-core/src/import/`.

### 16. Setup / Onboarding Wizard
- First-run flow: claim instance, configure AI provider, create default token.
- Needs: R, W.
- Data: registry settings, tokens.
- Anchors: `src/components/onboarding/OnboardingWizard.tsx:19`; route `crates/atomic-server/src/routes/setup.rs`.

---

## (b) AI / LLM Features

### 17. Chunking + Embedding Pipeline (background)
- On atom save: chunk content, embed each chunk, store vectors; progress via WebSocket.
- Needs: W, EMB, BG, WS.
- Data: atom_chunks, vec_chunks.
- Anchors: core `crates/atomic-core/src/embedding.rs:72 generate_embeddings_with_config`, `embedding.rs:1012 spawn_embedding_task_single`, `chunking.rs`; routes `crates/atomic-server/src/routes/embedding.rs:25 process_pending_embeddings`; frontend `src/hooks/useEmbeddingEvents.ts:56`.

### 18. Auto-Tagging
- LLM extracts tags from atom content and assigns under configured hierarchy targets.
- Needs: LLM, W, BG, WS.
- Data: atoms, tags, atom_tags.
- Anchors: core `crates/atomic-core/src/embedding.rs:640 process_tagging_only`, `embedding.rs:900 process_tagging_batch`, `extraction.rs:293 extract_tags_from_content`; frontend config `src/components/settings/SettingsModal.tsx:111`, events `src/hooks/useEmbeddingEvents.ts:141`.

### 19. Semantic Search
- Vector search over chunked content (sqlite-vec); hybrid keyword via RRF merge.
- Needs: R, EMB (query).
- Data: vec_chunks, atoms.
- Anchors: `src/components/search/SemanticSearch.tsx:23`; routes `crates/atomic-server/src/routes/search.rs:45 search`, `search.rs:82 global_search`; core `crates/atomic-core/src/search.rs:185 search_chunks`, `search.rs:115 merge_search_results_rrf`.

### 20. Wiki Synthesis
- LLM article summarizing all atoms under a tag with inline citations; incremental updates, versions, proposals.
- Needs: R, LLM, W, BG.
- Data: wiki_articles, atoms, tags, atom_chunks.
- Anchors: `src/components/wiki/WikiFullView.tsx:7`, `WikiReader.tsx`; routes `crates/atomic-server/src/routes/wiki.rs:36 generate_wiki`, `wiki.rs:49 update_wiki`; core `crates/atomic-core/src/wiki/mod.rs:112 strategy_generate`, `wiki/mod.rs:124 strategy_update`, `wiki/section_ops.rs`.

### 21. Agentic Chat / RAG
- Conversational tool-calling agent that semantically searches the KB; tag-scoped; token streaming.
- Needs: R, LLM, W, WS.
- Data: conversations, chat_messages, atom_chunks.
- Anchors: `src/components/chat/ChatView.tsx:12`; route `crates/atomic-server/src/routes/chat.rs:148 send_chat_message`; core `crates/atomic-core/src/chat.rs`, `wiki/agentic.rs`; events `src/hooks/useChatEvents.ts:33`.

### 22. Reports / Scheduled Research Tasks
- Recurring agentic tasks (briefings, contradiction scans, open-question tracking) producing cited findings.
- Needs: R, LLM, W, BG.
- Data: reports, findings, finding_citations.
- Anchors: `src/components/reports/ReportsFullView.tsx:18`; routes `crates/atomic-server/src/routes/reports.rs:62 create_report`, `reports.rs:243 run_report_now`; core `crates/atomic-core/src/reports/runner.rs:48 run_report`, `reports/scheduler/`, `reports/agentic.rs`.

### 23. Wiki Proposal / Diff Review
- Shows diff between current and LLM-proposed wiki version; accept or dismiss.
- Needs: R, LLM, W.
- Data: wiki_articles (proposal).
- Anchors: `src/components/wiki/WikiProposalDiff.tsx:99`; routes `wiki.rs:137 propose_wiki`, `wiki.rs:182 accept_wiki_proposal`, `wiki.rs:200 dismiss_wiki_proposal`; core `crates/atomic-core/src/wiki/mod.rs:159 strategy_propose`.

### 24. Tag Compaction / Consolidation
- LLM-assisted merge of duplicate/near-duplicate tags into a canonical form.
- Needs: LLM, R, W.
- Data: tags, atom_tags.
- Anchors: core `crates/atomic-core/src/compaction.rs`.

### 25. PCA 2D Projection
- Projects embedding vectors to 2D (PCA) to seed canvas positions for unpositioned atoms.
- Needs: R.
- Data: vec_chunks.
- Anchors: core `crates/atomic-core/src/projection.rs:11 compute_2d_projection`.

---

## (c) Ingest / Import Features

### 26. URL Ingestion (ingest_url)
- Fetches a web page, extracts readable content, creates an atom; also an MCP tool.
- Needs: W, EMB, LLM (pipeline), BG.
- Data: atoms.
- Anchors: routes `crates/atomic-server/src/routes/ingest.rs:30 ingest_url`, `ingest.rs:52 ingest_urls`; core `crates/atomic-core/src/ingest/fetch.rs:105 resolve_url`, `ingest/mod.rs`; frontend `src/components/dashboard/CaptureOptions.tsx:46`.

### 27. RSS Feed Ingest
- Subscribe to feeds; server polls on a background loop and creates new articles as atoms.
- Needs: R, W, BG, WS.
- Data: feeds, atoms.
- Anchors: routes `crates/atomic-server/src/routes/feeds.rs:21 create_feed`, `feeds.rs:59 poll_feed`; core `crates/atomic-core/src/ingest/rss.rs:22 parse_feed`; background `crates/atomic-server/src/main.rs`.

### 28. Browser Extension Capture (Web Clipper)
- Chrome extension captures page/selection and POSTs to `/api/atoms`; offline queue syncs every 30s.
- Needs: W (via create_atom), BG.
- Data: atoms (with source_url).
- Anchors: `extension/background/service-worker.js:36 captureContent`, `service-worker.js:80 sendToDesktop`; server `crates/atomic-server/src/routes/atoms.rs:332 create_atom`.

### 29. Bulk Atom Create
- Batch endpoint for programmatic bulk ingestion (scripts, RSS).
- Needs: W, BG.
- Data: atoms.
- Anchors: route `crates/atomic-server/src/routes/atoms.rs:373 bulk_create_atoms`.

### 30. Source URL Tracking + Dedup
- Atoms store optional source_url; lookup-by-URL avoids duplicate ingestion.
- Needs: R.
- Data: atoms.source_url.
- Anchors: routes `atoms.rs:293 get_atom_by_source_url`, `atoms.rs:202 get_source_list`.

---

## (d) Organizational / Infrastructure Features

### 31. Multi-Database Support
- Multiple isolated knowledge bases under one server via shared registry; create/rename/delete/switch.
- Needs: R, W.
- Data: registry.db databases table; per-DB SQLite files.
- Anchors: `src/components/DatabaseSwitcher.tsx:5`; routes `crates/atomic-server/src/routes/databases.rs:10 list_databases`, `databases.rs:27 create_database`, `databases.rs:68 activate_database`, `databases.rs:92 database_stats`; core `crates/atomic-core/src/manager.rs`.

### 32. Settings / AI Provider Config
- Configure provider (OpenRouter/Ollama/OpenAI-compat), per-feature model selection, test connections.
- Needs: R, W, LLM (test).
- Data: settings (global + per-DB).
- Anchors: `src/components/settings/SettingsModal.tsx:901`; routes `crates/atomic-server/src/routes/settings.rs:27 get_settings`, `settings.rs:38 set_setting`, `settings.rs:147 test_openrouter_connection`, `settings.rs:231 get_available_llm_models`; core `crates/atomic-core/src/providers/mod.rs:223 create_embedding_provider`, `mod.rs:253 create_llm_provider`.

### 33. API Token Management
- Create named, revocable Bearer tokens for clients (iOS, extension, MCP, scripts).
- Needs: R, W.
- Data: registry.db token hashes.
- Anchors: routes `crates/atomic-server/src/routes/auth.rs:16 create_token`, `auth.rs:38 list_tokens`, `auth.rs:47 revoke_token`; core `crates/atomic-core/src/tokens.rs:48 create_token`, `tokens.rs:76 list_tokens`.

### 34. MCP Server
- Exposes KB to Claude/AI tools over Streamable HTTP at `/mcp`. Tools: semantic_search, read_atom, create_atom, update_atom (+patch), ingest_url.
- Needs: R, W, EMB, LLM (downstream), BG.
- Data: atoms and all KB data.
- Anchors: `crates/atomic-server/src/mcp/server.rs:68 semantic_search`, `server.rs:115 read_atom`, `server.rs:174 create_atom`, `server.rs:220 ingest_url`, `server.rs:281 update_atom`; transport `mcp/transport.rs`; OAuth `routes/oauth.rs`; desktop bridge `crates/mcp-bridge/`.

### 35. WebSocket Real-Time Event Stream
- Live push for embedding/tagging completion, chat deltas, feed status, atom mutations.
- Needs: WS.
- Data: all (event metadata).
- Anchors: route `crates/atomic-server/src/routes/... ws.rs:7 ws_handler`; broadcast `crates/atomic-server/src/state.rs`, bridge `event_bridge.rs`; frontend `src/hooks/useEmbeddingEvents.ts`, `src/hooks/useChatEvents.ts`.

### 36. Command Palette + Search Palette
- Keyboard command palette (actions/nav) and quick-open search palette (semantic search, `#` tag scoping).
- Needs: R (search palette calls search).
- Data: atoms, tags.
- Anchors: `src/components/command-palette/useCommandPalette.ts:14`, `src/components/search-palette/SearchPalette.tsx`; trigger `src/components/layout/Layout.tsx:30`.

### 37. Dashboard
- Landing view with configurable widgets (featured report, recent atoms, wiki highlights).
- Needs: R.
- Data: reports, wiki_articles, atoms.
- Anchors: `src/components/dashboard/DashboardView.tsx:7`; registry `src/components/dashboard/registry.ts:15`.

### 38. Mobile (iOS / Android)
- Native mobile clients via Capacitor wrapping the React frontend; native SwiftUI iOS app.
- Needs: same REST API.
- Data: all (via API).
- Anchors: `mobile/ios/`, `mobile/android/`; responsive `src/hooks/useIsMobile.ts`.

### 39. Ollama Integration (local model discovery)
- Auto-discovers models from a running Ollama server for embeddings and LLM tasks.
- Needs: R.
- Data: settings.
- Anchors: route `crates/atomic-server/src/routes/ollama.rs`; core `crates/atomic-core/src/providers/`.

### 40. OAuth (remote MCP auth)
- OAuth 2.0 metadata endpoints for Claude.ai remote MCP connection (requires --public-url).
- Needs: R, W.
- Data: registry tokens.
- Anchors: `crates/atomic-server/src/routes/oauth.rs:26` (protected-resource metadata), `oauth.rs:65` (authorization-server metadata); auth `crates/atomic-server/src/mcp_auth.rs`.
