"""Atomic-owned Claude candidate preparation, ported from Session Survivor.

See README.md for source attribution and LICENSE for the retained GPLv3 text.
"""
from __future__ import annotations

import copy
import hashlib
import json
import re
from collections import defaultdict
from dataclasses import dataclass
from types import SimpleNamespace
from typing import Any


TOOL_OUTPUT_PLACEHOLDER = "[Compacted Claude tool result"
TOOL_USE_TYPES = ("tool_use", "server_tool_use", "mcp_tool_use")


COMMAND_WRAPPER_RE = re.compile(
    r"^\s*<command-name>.*?</command-name>\s*"
    r"<command-message>.*?</command-message>\s*"
    r"<command-args>.*?</command-args>\s*$",
    re.DOTALL,
)
CONTINUATION_SUMMARY_PREFIX = "This session is being continued from a previous conversation that ran out of context."


def extract_message_text(content: Any) -> str:
    if isinstance(content, str):
        return content.strip()
    if not isinstance(content, list):
        return ""

    chunks: list[str] = []
    for item in content:
        if not isinstance(item, dict):
            continue
        if item.get("type") != "text":
            continue
        text = item.get("text")
        if isinstance(text, str) and text.strip():
            chunks.append(text.strip())
    return "\n\n".join(chunks).strip()


def is_meta_noise(text: str) -> bool:
    normalized = text.strip()
    if not normalized:
        return False
    if normalized.startswith("<local-command-caveat>") and normalized.endswith("</local-command-caveat>"):
        return True
    if normalized.startswith("<local-command-stdout>") and normalized.endswith("</local-command-stdout>"):
        return True
    if normalized.startswith("<local-command-stderr>") and normalized.endswith("</local-command-stderr>"):
        return True
    if COMMAND_WRAPPER_RE.match(normalized):
        return True
    if normalized.startswith("<task-notification>"):
        return True
    return False


def is_continuation_summary(text: str) -> bool:
    normalized = text.strip()
    if not normalized.startswith(CONTINUATION_SUMMARY_PREFIX):
        return False
    return "\n\nSummary:" in normalized


def is_user_turn_start(obj: dict[str, Any]) -> bool:
    if obj.get("type") != "user" or obj.get("isCompactSummary"):
        return False
    message = obj.get("message")
    if not isinstance(message, dict) or message.get("role") != "user":
        return False
    content = message.get("content")
    if isinstance(content, list) and any(
        isinstance(block, dict) and block.get("type") == "tool_result" for block in content
    ):
        return False
    text = extract_message_text(content)
    return bool(text and not is_meta_noise(text) and not is_continuation_summary(text))


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def is_existing_compaction_placeholder(text: str) -> bool:
    return (
        "\n... [Compacted Claude" in text
        and "; original length=" in text
        and text.rstrip().endswith(" chars]")
    )


def shorten(text: str, max_chars: int, label: str) -> tuple[str, bool]:
    if is_existing_compaction_placeholder(text):
        return text, False
    if len(text) <= max_chars:
        return text, False
    kept = text[:max_chars].rstrip()
    compacted = f"{kept}\n... {label}; original length={len(text)} chars]"
    return compacted, True


def compact_message_content(item: dict[str, Any], args: SimpleNamespace, state: dict[str, int]) -> dict[str, Any]:
    out = copy.deepcopy(item)
    item_type = out.get("type")

    if item_type == "tool_result":
        content = out.get("content")
        if isinstance(content, str):
            compacted, changed = shorten(content, args.max_tool_output_chars, TOOL_OUTPUT_PLACEHOLDER)
            out["content"] = compacted
            if changed:
                state["tool_outputs_truncated"] += 1
        elif (isinstance(content, list)
              and all(isinstance(part, dict) and part.get("type") == "text"
                      and isinstance(part.get("text"), str) for part in content)):
            for part in content:
                part["text"], changed = shorten(part["text"], args.max_tool_output_chars,
                                                TOOL_OUTPUT_PLACEHOLDER)
                if changed:
                    state["tool_outputs_truncated"] += 1

    return out


def compact_record(obj: dict[str, Any], args: SimpleNamespace, state: dict[str, int]) -> dict[str, Any]:
    item = copy.deepcopy(obj)
    item_type = item.get("type")

    if item_type in ("assistant", "user"):
        message = item.get("message")
        if isinstance(message, dict):
            content = message.get("content")
            if isinstance(content, list):
                compacted_content: list[Any] = []
                for entry in content:
                    if isinstance(entry, dict) and entry.get("type") == "thinking":
                        state["thinking_blocks_removed"] += 1
                        continue
                    if isinstance(entry, dict):
                        compacted_content.append(compact_message_content(entry, args, state))
                    else:
                        compacted_content.append(entry)
                message["content"] = compacted_content
            # Plain user/assistant strings are dialogue, not tool output.

    return item


def validate_claude_records(records: list[dict[str, Any]]) -> None:
    uuids: set[str] = set()
    tool_uses: dict[str, set[str]] = {}
    tool_results: list[tuple[str, str]] = []
    meaningful = False

    for row in records:
        row_uuid = row.get("uuid")
        if isinstance(row_uuid, str) and row_uuid:
            if row_uuid in uuids:
                raise ValueError(f"Duplicate Claude UUID: {row_uuid}")
            uuids.add(row_uuid)
            if row.get("parentUuid") == row_uuid:
                raise ValueError(f"Claude row is its own parent: {row_uuid}")

        if row.get("type") not in ("user", "assistant"):
            continue
        if not isinstance(row_uuid, str) or not row_uuid.strip():
            raise ValueError("Claude dialogue has no valid row UUID.")
        message = row.get("message")
        if not isinstance(message, dict) or message.get("role") != row["type"]:
            raise ValueError("Claude dialogue role must match its record type.")
        content = message.get("content")
        if isinstance(content, str):
            meaningful = meaningful or bool(content.strip())
            continue
        if not isinstance(content, list):
            continue
        for block in content:
            if not isinstance(block, dict) or block.get("type") == "thinking":
                continue
            block_type = block.get("type")
            if block_type == "text":
                meaningful = meaningful or bool(str(block.get("text", "")).strip())
            else:
                meaningful = True
            if block_type in TOOL_USE_TYPES:
                if not isinstance(block.get("id"), str) or not block["id"].strip():
                    raise ValueError("Claude tool invocation has no valid ID.")
                tool_uses.setdefault(row_uuid, set()).add(block["id"])
            if block_type == "tool_result":
                if not isinstance(block.get("tool_use_id"), str) or not block["tool_use_id"].strip():
                    raise ValueError("Claude tool result has no valid invocation ID.")
                tool_results.append((row_uuid, block["tool_use_id"]))

    if not meaningful:
        raise ValueError("No meaningful Claude dialogue survived filtering.")
    missing_parents = {
        row["parentUuid"] for row in records
        if isinstance(row.get("parentUuid"), str) and row["parentUuid"] and row["parentUuid"] not in uuids
    }
    if missing_parents:
        raise ValueError(f"Claude output has unresolved parent UUIDs: {len(missing_parents)}")
    by_uuid = {row["uuid"]: row for row in records if isinstance(row.get("uuid"), str) and row["uuid"]}
    for row in records:
        if row.get("type") != "system" or row.get("subtype") != "compact_boundary":
            continue
        metadata = row.get("compactMetadata") or {}
        segment = metadata.get("preservedSegment") or {}
        preserved = metadata.get("preservedMessages") or {}
        for key in ("uuids", "allUuids"):
            refs = preserved.get(key)
            if key in preserved and (not isinstance(refs, list)
                                     or any(not isinstance(ref, str) or not ref.strip() for ref in refs)):
                label = "advisory allUuids" if key == "allUuids" else "replay uuids"
                raise ValueError(f"Claude compact {label} must be a list of nonempty strings.")
        references = [row.get("logicalParentUuid"), preserved.get("anchorUuid")]
        references.extend(segment.get(key) for key in ("headUuid", "anchorUuid", "tailUuid"))
        references.extend(preserved.get("uuids", []))
        # Native replay filters missing advisory allUuids; uuids/anchors must resolve.
        if any(ref is not None and (not isinstance(ref, str) or ref not in uuids) for ref in references):
            raise ValueError("Claude output has unresolved compact UUID references; retain a larger native tail.")
    checked: set[str] = set()
    for start in by_uuid:
        if start in checked:
            continue
        seen: set[str] = set()
        current: str | None = start
        while current in by_uuid and current not in checked:
            if current in seen:
                raise ValueError("Claude output has a parent cycle.")
            seen.add(current)
            row = by_uuid[current]
            parent = row.get("parentUuid")
            if parent is None and row.get("type") == "system" and row.get("subtype") == "compact_boundary":
                parent = row.get("logicalParentUuid")
            current = parent if isinstance(parent, str) and parent else None
        checked.update(seen)
    for row_uuid, tool_id in tool_results:
        parent = by_uuid[row_uuid].get("parentUuid")
        while isinstance(parent, str) and parent in by_uuid:
            if tool_id in tool_uses.get(parent, set()):
                break
            parent = by_uuid[parent].get("parentUuid")
        else:
            raise ValueError("Claude output has a tool result without an invocation in its parent chain.")


@dataclass(frozen=True)
class AtomicSessionCandidate:
    candidate_bytes: bytes
    source_sha256: str
    candidate_sha256: str
    changes: dict[str, int]


def prepare_atomic_session(source_bytes: bytes) -> AtomicSessionCandidate:
    """Prepare a settled, writer-idle transcript; the caller owns adoption/proof.

    Keep unchanged lines byte-identical. Reduce only older primary message
    content; retain metadata and supporting records.
    """
    lines = source_bytes.splitlines(keepends=True)
    records = [json.loads(line) for line in lines]
    if any(not isinstance(record, dict) for record in records):
        raise ValueError("Claude session records must be JSON objects")
    validate_claude_records(records)
    # A failed recap or meta injection must not displace the last answered turn.
    latest_start = 0
    pending_start = None
    answered = False
    pending_calls: set[str] = set()
    unclear = False
    for index, record in enumerate(records):
        if record.get("isSidechain") or record.get("isMeta"):
            continue
        if is_user_turn_start(record):
            if pending_start is not None and answered:
                latest_start = pending_start
            pending_start, answered = index, False
            unclear = False
        if pending_start is None or record.get("type") not in ("user", "assistant"):
            continue
        message = record["message"]
        content = message.get("content")
        if isinstance(content, list):
            for block in content:
                if not isinstance(block, dict):
                    unclear = True
                elif block.get("type") in TOOL_USE_TYPES:
                    pending_calls.add(block["id"])
                    answered = False
                elif block.get("type") == "tool_result":
                    if block["tool_use_id"] not in pending_calls:
                        unclear = True
                    pending_calls.discard(block["tool_use_id"])
                elif block.get("type") not in ("text", "thinking", "image", "document"):
                    unclear = True
        elif not isinstance(content, str):
            unclear = True
        if pending_calls or unclear:
            answered = False
        elif record.get("type") == "assistant":
            # Commentary is not completion, even if its calls settle in a later row.
            if (record.get("isApiErrorMessage") or record.get("error")
                    or message.get("stop_reason") not in (None, "end_turn", "stop_sequence")):
                answered = False
            elif extract_message_text(content):
                answered = True
    if pending_start is not None and answered:
        latest_start = pending_start
    args = SimpleNamespace(max_tool_output_chars=400)
    changes: dict[str, int] = defaultdict(int)
    output = []
    for index, (record, line) in enumerate(zip(records, lines)):
        candidate = record
        if index < latest_start and not record.get("isSidechain") and record.get("type") in ("user", "assistant"):
            message = record["message"]
            content = message.get("content")
            reduced = compact_record({"type": record["type"], "message": {"content": content}},
                                     args, changes)["message"]["content"]
            if reduced != content:
                candidate = copy.deepcopy(record)
                candidate["message"]["content"] = reduced
                ending = b"\r\n" if line.endswith(b"\r\n") else b"\n" if line.endswith(b"\n") else b""
                line = json.dumps(candidate, ensure_ascii=True, separators=(",", ":")).encode("utf-8") + ending
                changes["records_changed"] += 1
        output.append(line)
    candidate_bytes = b"".join(output)
    return AtomicSessionCandidate(candidate_bytes, sha256_bytes(source_bytes),
                                  sha256_bytes(candidate_bytes), dict(changes))
