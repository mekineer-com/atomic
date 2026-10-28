# Atomic Claude Session Cleaner

Atomic owns this Python helper and its tests. It is hunk-ported from
[`mekineer-com/session-survivor`](https://github.com/mekineer-com/session-survivor)'s
Claude transformer, validator and turn classifier, with Atomic-specific preservation.
The upstream tools are reference material, not a runtime dependency or a place
for Atomic changes. The original GPLv3 license is retained in `LICENSE`.

```python
from atomic_claude_session import prepare_atomic_session
candidate = prepare_atomic_session(closed_session_bytes)
```

Returns candidate bytes, source/candidate hashes and change counts. It does not
read/adopt a file, establish writer exit or estimate token savings. The caller
owns those lifecycle checks. Dialogue, Presence, native identities and the last
answered non-error turn stay intact, including any later failed/unanswered prompts.

From the Atomic checkout:

```sh
python3 -m unittest discover -s scripts/claude_session -p 'test_*.py'
```

The local `pyproject.toml` packages only this callable. Installation and native
runtime integration remain separate from these offline preservation tests.
