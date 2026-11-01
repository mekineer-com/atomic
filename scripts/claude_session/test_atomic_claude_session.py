import copy
import hashlib
import json
import subprocess
import sys
import unittest
from collections import defaultdict
from pathlib import Path
from types import SimpleNamespace

from atomic_claude_session import compact_record, prepare_atomic_session, validate_claude_records


def row(kind, uuid, parent, content):
    return {"type": kind, "uuid": uuid, "parentUuid": parent,
            "message": {"role": kind, "content": content}}


class ClaudeSafePreservationTest(unittest.TestCase):
    def test_atomic_profile_preserves_dialogue_envelopes_and_latest_turn(self):
        long = "fictional full dialogue and result " * 300
        for as_blocks, ending in ((False, b"\n"), (True, b"\r\n")):
            with self.subTest(as_blocks=as_blocks):
                text = lambda value: [{"type": "text", "text": value}] if as_blocks else value
                records = [
                    {"type": "attachment", "uuid": "presence", "parentUuid": None,
                     "attachment": {"type": "prompt_snapshot", "content": long}},
                    {"type": "agent-setting", "agentSetting": "fictional-persona", "sessionId": "fictional"},
                    row("user", "u0", "presence", text(long)),
                    row("assistant", "a0", "u0", [
                        {"type": "thinking", "thinking": long, "signature": "whole signature"},
                        {"type": "text", "text": long},
                        *[{"type": "tool_use", "id": name, "name": "fictional_tool", "input": {"text": long}}
                          for name in ("plain", "blocks", "unknown")],
                    ]),
                    row("user", "r0", "a0", [
                        {"type": "text", "text": "Mixed dialogue stays verbatim"},
                        {"type": "tool_result", "tool_use_id": "plain", "content": long, "is_error": True},
                        {"type": "tool_result", "tool_use_id": "blocks", "content": [{"type": "text", "text": long}]},
                        {"type": "tool_result", "tool_use_id": "unknown", "content": [
                            {"type": "text", "text": long}, {"type": "future_result", "data": long}]},
                    ]),
                    row("assistant", "a_done", "r0", text(long)),
                    row("user", "u1", "a_done", text("<atomic_turn_context>scope</atomic_turn_context>\n" + long)),
                    row("assistant", "a1", "u1", [
                        {"type": "thinking", "thinking": long, "signature": "latest signature"},
                        {"type": "text", "text": long},
                    ]),
                    {"type": "future-support", "uuid": "support", "parentUuid": "a1", "opaque": long},
                ]
                records.extend([
                    dict(row("user", "side_user", "a1", text(long)), isSidechain=True),
                    dict(row("assistant", "side_answer", "side_user", text(long)), isSidechain=True),
                ])
                for record in records:
                    if record["type"] == "assistant":
                        record["message"].update(model="fictional-model", usage={"output_tokens": 10, "future_usage": long})
                records[4]["toolUseResult"] = {"unknown": long}
                lines = [json.dumps(record, ensure_ascii=False).encode() + ending for record in records]
                source = b"".join(lines)
                result = prepare_atomic_session(source)
                output = [json.loads(line) for line in result.candidate_bytes.splitlines()]
                self.assertEqual(output[2], records[2])
                self.assertEqual(output[5:], records[5:])
                self.assertEqual(output[3]["message"]["content"], records[3]["message"]["content"][1:])
                self.assertEqual(output[3]["message"]["usage"], records[3]["message"]["usage"])
                blocks = output[4]["message"]["content"]
                self.assertEqual(blocks[0], records[4]["message"]["content"][0])
                self.assertEqual(blocks[3], records[4]["message"]["content"][3])
                self.assertTrue(blocks[1]["is_error"])
                self.assertEqual(output[4]["toolUseResult"], records[4]["toolUseResult"])
                self.assertIn("original length=", blocks[1]["content"])
                self.assertIn("original length=", blocks[2]["content"][0]["text"])
                candidate_lines = result.candidate_bytes.splitlines(keepends=True)
                for index in (0, 1, 2, 5, 6, 7, 8, 9, 10):
                    self.assertEqual(candidate_lines[index], lines[index])
                self.assertEqual(result.source_sha256, hashlib.sha256(source).hexdigest())
                self.assertEqual(result.candidate_sha256, hashlib.sha256(result.candidate_bytes).hexdigest())
                self.assertEqual(result.changes, {"thinking_blocks_removed": 1, "tool_outputs_truncated": 2, "records_changed": 2})
                self.assertEqual(prepare_atomic_session(result.candidate_bytes).candidate_bytes,
                                 result.candidate_bytes)
                self.assertEqual(prepare_atomic_session(result.candidate_bytes).changes, {})

    def test_atomic_profile_protects_summary_only_sessions_and_rejects_broken_graphs(self):
        records = [row("user", "u", None, "Retained summary"),
                   row("assistant", "a", "u", [{"type": "thinking", "thinking": "retained", "signature": "complete"}])]
        records[0]["isCompactSummary"] = True
        source = b"\n".join(json.dumps(record).encode() for record in records)
        self.assertEqual(prepare_atomic_session(source).candidate_bytes, source)
        records[1]["parentUuid"] = "missing"
        with self.assertRaisesRegex(ValueError, "parent"):
            prepare_atomic_session(b"\n".join(json.dumps(record).encode() for record in records))
        for malformed in (b'[]', b'null', b'\n', b'{"type":'):
            with self.subTest(malformed=malformed), self.assertRaises(ValueError):
                prepare_atomic_session(malformed)

    def test_atomic_profile_keeps_latest_completed_turn_before_failed_recap(self):
        for error in (None, {"isApiErrorMessage": True}, {"error": "invalid_request"}):
            with self.subTest(error=error):
                records = [
                    row("user", "old_user", None, "Older question"),
                    row("assistant", "old_thinking", "old_user", [
                        {"type": "thinking", "thinking": "old bulk", "signature": "whole"}]),
                    row("assistant", "old_answer", "old_thinking", "Older answer"),
                    {"type": "system", "subtype": "compact_boundary", "uuid": "boundary",
                     "parentUuid": None, "logicalParentUuid": "old_answer",
                     "compactMetadata": {"preservedMessages": {"uuids": ["old_user"]}}},
                    dict(row("user", "summary", "boundary", "Native summary"), isCompactSummary=True),
                    row("user", "latest_user", "summary", "Latest successful question"),
                    row("assistant", "latest_thinking", "latest_user", [
                        {"type": "thinking", "thinking": "latest bulk", "signature": "keep whole"}]),
                    dict(row("user", "meta", "latest_thinking", "Injected meta context"), isMeta=True),
                    row("assistant", "latest_use", "meta", [
                        {"type": "tool_use", "id": "latest-call", "name": "fictional", "input": {}}]),
                    row("user", "latest_result", "latest_use", [
                        {"type": "tool_result", "tool_use_id": "latest-call", "content": "latest full result " * 100}]),
                    row("assistant", "latest_answer", "latest_result", "Latest successful answer"),
                    row("user", "recap", "latest_answer", "Tool-free recap prompt"),
                ]
                records[1]["message"]["id"] = records[2]["message"]["id"] = "old-message"
                records[6]["message"]["id"] = records[8]["message"]["id"] = "latest-message"
                records[1]["opaque"] = "Preserve escaped surrogate: \ud83d"
                if error is not None:
                    records.append(dict(row("assistant", "failure", "recap", "Context length failure"), **error))
                lines = [json.dumps(record).encode() + b"\n" for record in records]
                result = prepare_atomic_session(b"".join(lines))
                output_lines = result.candidate_bytes.splitlines(keepends=True)
                self.assertEqual(output_lines[3:], lines[3:])
                old_thinking = json.loads(output_lines[1])
                self.assertEqual(old_thinking["message"]["content"], [])
                self.assertEqual(old_thinking["opaque"], records[1]["opaque"])
                self.assertEqual(old_thinking["message"]["id"], "old-message")
                self.assertEqual(result.changes, {"thinking_blocks_removed": 1, "records_changed": 1})
                self.assertEqual(prepare_atomic_session(result.candidate_bytes).candidate_bytes, result.candidate_bytes)

    def test_unfinished_commentary_does_not_displace_completed_exchange(self):
        for tail in ("pending", "settled", "partial", "complete", "unknown", "truncated", "user_unknown", "user_unknown_text",
                     "later_complete", "later_pending", "late_old_result"):
            with self.subTest(tail=tail):
                records = [
                    row("user", "u0", None, "Completed question"),
                    row("assistant", "think0", "u0", [
                        {"type": "thinking", "thinking": "Keep completed reasoning", "signature": "whole"}]),
                    row("assistant", "use0", "think0", [
                        {"type": "tool_use", "id": "call0", "name": "fictional", "input": {}}]),
                    row("user", "result0", "use0", [
                        {"type": "tool_result", "tool_use_id": "call0", "content": "x" * 900}]),
                    row("assistant", "answer0", "result0", "Completed answer"),
                    row("user", "u1", "answer0", "Next question"),
                    row("assistant", "comment", "u1", [
                        {"type": "text", "text": "I will inspect this."},
                        {"type": "tool_use", "id": "call1", "name": "fictional", "input": {}},
                        {"type": "tool_use", "id": "call2", "name": "fictional", "input": {}}]),
                ]
                if tail in {"user_unknown", "user_unknown_text"}:
                    records[-1]["message"]["content"] = [{"type": "text", "text": "I will inspect this."}]
                    blocks = [{"type": "future_tool_result", "opaque": True}]
                    if tail == "user_unknown_text":
                        blocks.append({"type": "text", "text": "Unknown continuation"})
                    records.append(row("user", "unknown", "comment", blocks))
                if tail in {"settled", "partial", "complete", "unknown", "truncated"}:
                    results = [{"type": "tool_result", "tool_use_id": "call1", "content": "First"}]
                    if tail != "partial":
                        results.append({"type": "tool_result", "tool_use_id": "call2", "content": "Second"})
                    records.append(row("user", "results1", "comment", results))
                if tail in {"partial", "complete", "unknown", "truncated"}:
                    content = [{"type": "text", "text": "Final text"}]
                    if tail == "unknown":
                        content.append({"type": "future_call", "opaque": True})
                    records.append(row("assistant", "answer1", records[-1]["uuid"], content))
                    if tail == "truncated":
                        records[-1]["message"]["stop_reason"] = "max_tokens"
                if tail in {"later_complete", "later_pending", "late_old_result"}:
                    records.extend([
                        row("user", "u2", "comment", "A fresh request after abandoned work"),
                        row("assistant", "think2", "u2", [
                            {"type": "thinking", "thinking": "Newest reasoning", "signature": "whole"}]),
                        row("assistant", "use2", "think2", [
                            {"type": "text", "text": "I will check."},
                            {"type": "tool_use", "id": "call3", "name": "fictional", "input": {}}]),
                    ])
                    if tail != "later_pending":
                        records.extend([
                            row("user", "result2", "use2", [
                                {"type": "tool_result", "tool_use_id": "call3", "content": "y" * 900}]),
                            row("assistant", "answer2", "result2", "A genuinely completed answer"),
                        ])
                    if tail == "late_old_result":
                        records.append(row("user", "late_result", "answer2", [
                            {"type": "tool_result", "tool_use_id": "call1", "content": "Old result arrived late"}]))
                records.append(row("user", "recap", records[-1]["uuid"], "Tool-free recap prompt"))
                records.append(dict(row("assistant", "failure", "recap", "Context failure"), isApiErrorMessage=True))
                source = [json.dumps(record).encode() + b"\n" for record in records]
                output = prepare_atomic_session(b"".join(source)).candidate_bytes.splitlines(keepends=True)
                if tail in {"complete", "later_complete"}:
                    self.assertEqual(json.loads(output[1])["message"]["content"], [])
                    if tail == "later_complete":
                        self.assertEqual(output[7:], source[7:])
                else:
                    self.assertEqual(output, source)

    def test_atomic_import_and_preparation_do_not_load_cli_or_posix_modules(self):
        code = '''
import importlib.abc, sys
class RejectCLI(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, *args):
        if fullname in {"artifact_publish", "lineage", "fcntl", "chat_claude_session"}:
            raise AssertionError("CLI dependency imported: " + fullname)
sys.meta_path.insert(0, RejectCLI())
from atomic_claude_session import prepare_atomic_session
result = prepare_atomic_session(b'{"type":"user","uuid":"fictional","message":{"role":"user","content":"hello"}}')
assert result.changes == {}
'''
        subprocess.run([sys.executable, "-c", code], check=True, capture_output=True,
                       cwd=Path(__file__).parent)

    def test_agent_settings_are_preserved_unchanged(self):
        for selection in ("fictional-alpha", "fictional-beta", ""):
            record = {"type": "agent-setting", "agentSetting": selection,
                      "sessionId": "fictional"}
            output = compact_record(record, SimpleNamespace(), defaultdict(int))
            self.assertEqual(output, record)
            self.assertIsNot(output, record)

    def test_invocation_families_require_ids_and_ancestry(self):
        for family in ("tool_use", "server_tool_use", "mcp_tool_use"):
            records = [row("user", "u", None, "fictional question"),
                       row("assistant", "a", "u", [{"type": family, "id": "call"}]),
                       row("user", "r", "a", [{"type": "tool_result", "tool_use_id": "call"}])]
            validate_claude_records(records)
            for block, key in ((records[1]["message"]["content"][0], "id"),
                               (records[2]["message"]["content"][0], "tool_use_id")):
                for invalid in (None, "", " ", 42):
                    with self.subTest(family=family, key=key, invalid=invalid):
                        block[key] = invalid
                        with self.assertRaisesRegex(ValueError, "ID"):
                            validate_claude_records(records)
                del block[key]
                with self.assertRaisesRegex(ValueError, "ID"):
                    validate_claude_records(records)
                block[key] = "call"
            records[2]["parentUuid"] = "u"
            with self.assertRaisesRegex(ValueError, "parent chain"):
                validate_claude_records(records)

    def test_identity_and_role_without_rejecting_shells(self):
        prompt = row("user", "u", None, "fictional question")
        shell = row("assistant", "shell", "u", [])
        validate_claude_records([prompt, shell])
        shell["message"]["content"] = [{"type": "thinking", "thinking": "fictional", "signature": "fake"}]
        validate_claude_records([prompt, shell])
        for invalid in (None, "", " ", 42):
            bad = copy.deepcopy(prompt)
            bad["uuid"] = invalid
            with self.assertRaisesRegex(ValueError, "UUID"):
                validate_claude_records([bad])
        bad.pop("uuid")
        with self.assertRaisesRegex(ValueError, "UUID"):
            validate_claude_records([bad])
        for invalid in (None, {}, {"role": "assistant", "content": "fictional"}):
            bad = dict(prompt, message=invalid)
            with self.assertRaisesRegex(ValueError, "role"):
                validate_claude_records([bad])

    def test_replay_shape_and_missing_advisory_refs(self):
        prompt = row("user", "u", None, "fictional question")
        preserved = {"uuids": ["u"], "allUuids": ["absent-advisory"]}
        boundary = {"type": "system", "subtype": "compact_boundary", "uuid": "b",
                    "parentUuid": None, "logicalParentUuid": "u",
                    "compactMetadata": {"preservedMessages": preserved}}
        summary = row("user", "summary", "b", "fictional complete summary")
        summary["isCompactSummary"] = True
        records = [prompt, boundary, summary]
        validate_claude_records(records)
        for key in ("uuids", "allUuids"):
            for invalid in (None, "u", {}, [None], [42], [""], [" "]):
                with self.subTest(key=key, invalid=invalid):
                    preserved[key] = invalid
                    with self.assertRaisesRegex(ValueError, "list of nonempty strings"):
                        validate_claude_records(records)
            preserved[key] = []
            validate_claude_records(records)
            del preserved[key]
            validate_claude_records(records)
        preserved["uuids"] = ["missing-replay"]
        with self.assertRaisesRegex(ValueError, "compact UUID references"):
            validate_claude_records(records)

if __name__ == "__main__":
    unittest.main()
