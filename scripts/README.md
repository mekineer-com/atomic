# Atomic Scripts

This directory contains utility scripts for the Atomic application.

## Obsidian Import Script

The `import/obsidian.js` script imports notes from an Obsidian vault into Atomic.

### Features

- Imports all markdown files from an Obsidian vault
- Extracts tags from folder structure (e.g., `Projects/Work/note.md` → tags: "Projects", "Work")
- Extracts tags from YAML frontmatter
- Preserves `[[wikilinks]]` and `![[embeds]]` as-is in content
- Deduplicates by source URL (re-importing the same vault skips existing notes)
- Supports dry-run mode to preview what would be imported

### Usage

```bash
# From the app's Settings modal (recommended)
# Click "Import from Obsidian" and select your vault folder

# Or via CLI:
npm run import:obsidian /path/to/vault

# Import with a maximum number of notes
npm run import:obsidian /path/to/vault -- --max 100

# Dry run to see what would be imported
npm run import:obsidian /path/to/vault -- --dry-run

# Exclude additional patterns
npm run import:obsidian /path/to/vault -- --exclude "Templates/**"

# Custom database path
npm run import:obsidian /path/to/vault -- --db /path/to/atomic.db
```

### CLI Options

| Option | Description |
|--------|-------------|
| `--max <n>` | Maximum number of notes to import |
| `--exclude <pattern>` | Additional glob patterns to exclude (can use multiple times) |
| `--dry-run` | Show what would be imported without importing |
| `--json-output` | Output results as JSON (for programmatic use) |
| `--db <path>` | Custom database path |
| `-h, --help` | Show help message |

### Default Exclusions

The importer automatically excludes:
- `.obsidian/**` (Obsidian configuration)
- `.trash/**` (Obsidian trash)
- `.git/**` (Git repository)
- `node_modules/**`

### Tag Mapping

Tags are extracted from two sources:

1. **Folder structure**: Each folder level becomes a tag
   - `Projects/Work/meeting-notes.md` → Tags: "Projects", "Work"

2. **YAML frontmatter**: The `tags` field is extracted
   - Supports array format: `tags: [topic1, topic2]`
   - Supports comma-separated: `tags: topic1, topic2`

### Wikilinks

Obsidian's `[[wikilinks]]` and `![[embeds]]` are preserved as-is in the imported content. This allows for potential future linking features and keeps your notes readable.

### After Import

1. Open the Atomic app
2. Embeddings will process automatically in the background
3. Tags extracted from folders/frontmatter are linked immediately
4. AI auto-tagging (if enabled) will add additional tags

---

## Wikipedia Import Script

The `import-wikipedia.js` script fetches Wikipedia articles and imports them into the Atomic database for stress testing.

### Prerequisites

1. Install dependencies:
   ```bash
   npm install
   ```

2. Run the Atomic app at least once to create the database.

### Usage

```bash
# Import 500 articles (default)
npm run import:wikipedia

# Import a custom number of articles
npm run import:wikipedia 1000

# Specify a custom database path
npm run import:wikipedia 500 --db /path/to/atomic.db
```

### Topics

The script imports articles from three domains for diversity:

1. **Computing** (~200 articles)
   - History of computing, Alan Turing, Programming languages, AI, etc.

2. **Philosophy** (~200 articles)
   - Plato, Aristotle, Ethics, Metaphysics, Existentialism, etc.

3. **History** (~200 articles)
   - European history, World Wars, Ancient civilizations, etc.

### How It Works

1. Starts with seed articles from each domain
2. Fetches article summaries from Wikipedia's REST API
3. Follows related article links to discover more content
4. Inserts articles into the SQLite database with `embedding_status: 'pending'`
5. Respects rate limits (100ms delay between requests)

### After Import

When you open the Atomic app after importing:

1. The embedding pipeline will process all pending atoms
2. If auto-tagging is enabled, tags will be extracted using the configured model
3. Processing time depends on:
   - Number of imported articles
   - Your OpenRouter API rate limits
   - The configured tagging model (gpt-4o-mini is faster/cheaper)

### Database Location

The script automatically detects the database location based on your OS:

- **macOS**: `~/Library/Application Support/com.atomic.app/atomic.db`
- **Linux**: `~/.local/share/com.atomic.app/atomic.db`
- **Windows**: `%APPDATA%/com.atomic.app/atomic.db`

You can override this with the `--db` flag.

### Tips for Bulk Import

1. **Use a cheaper model**: Set the tagging model to `openai/gpt-4o-mini` in settings before importing
2. **Disable auto-tagging**: If you don't need tags, disable auto-tagging in settings to speed up processing
3. **Start small**: Test with 50-100 articles first to estimate processing time

---

## Chunk Reset Script

The `reset-chunks.js` script deletes all chunks and related data, then marks atoms for re-embedding. This is useful after changing the chunking strategy or embedding model.

### What It Deletes

- All chat conversations, messages, tool calls, and citations
- All wiki articles and citations
- All semantic edges and atom clusters
- All atom positions (canvas will re-simulate)
- All vector chunks and FTS entries
- All atom chunks

### What It Preserves

- All atoms (content is preserved)
- All tags and tag associations

### Usage

```bash
# Preview what would be deleted (recommended first step)
node scripts/reset-chunks.js --dry-run

# Reset with backup (recommended)
node scripts/reset-chunks.js --backup

# Reset without confirmation
node scripts/reset-chunks.js --force --backup

# Specify custom database path
node scripts/reset-chunks.js --db /path/to/atomic.db
```

### Options

| Option | Description |
|--------|-------------|
| `--dry-run` | Show what would happen without making changes |
| `--backup` | Create a backup before resetting |
| `--force` | Skip confirmation prompt |
| `--db <path>` | Custom database path |
| `--help` | Show help message |

### After Reset

1. Start the Atomic app
2. Go to Settings and click "Process Pending Embeddings"
3. Wait for all atoms to be re-embedded with the new chunking strategy

---

## Tag Reset Script

The `reset-tags.js` script resets all tags to default top-level categories and marks atoms for re-tagging.

### Usage

```bash
# Preview what would be deleted
node scripts/reset-tags.js --dry-run

# Reset with backup
node scripts/reset-tags.js --backup

# Reset without confirmation
node scripts/reset-tags.js --force --backup
```

### What It Does

1. Deletes all wiki articles and citations
2. Deletes all atom-tag associations
3. Deletes all tags and recreates default categories
4. Marks all atoms for re-tagging

---

## Database Location

All scripts automatically detect the database location based on your OS:

- **macOS**: `~/Library/Application Support/com.atomic.app/atomic.db`
- **Linux**: `~/.local/share/com.atomic.app/atomic.db`
- **Windows**: `%APPDATA%/com.atomic.app/atomic.db`

You can override this with the `--db` flag on any script.

