"""P0-only strict receipts and raw terminal protocol observations."""

import re


SUPPORT_REPLY = (b"\x1b]7501;?:states=idle,working,done,blocked,error"
                 b":kinds=permission,question,auth\x1b\\")
REVIEW_SCHEMA = {"type": "object", "properties": {
    "checkpoint": {"type": "string"}, "verdict": {"type": "string", "enum": ["approved", "needs-fix"]},
    "findings": {"type": "array", "items": {"type": "string"}}},
    "required": ["checkpoint", "verdict", "findings"], "additionalProperties": False}


def validate_review(value, checkpoint):
    if not isinstance(value, dict) or set(value) != {"checkpoint", "verdict", "findings"}:
        raise ValueError("Invalid receipt fields")
    if value["checkpoint"] != checkpoint:
        raise ValueError("Stale receipt checkpoint")
    if value["verdict"] not in ("approved", "needs-fix"):
        raise ValueError("Invalid verdict")
    if not isinstance(value["findings"], list) or not all(isinstance(x, str) for x in value["findings"]):
        raise ValueError("Invalid findings")
    return value


def verify_claude_denial(output, tool_id, forbidden, control_denials=(), tool_results=()):
    for denial in output.get("permission_denials", []):
        if denial.get("tool_name") == "Write" and denial.get("tool_use_id") == tool_id and \
                denial.get("tool_input", {}).get("file_path") == forbidden:
            return True
    requested = any(d.get("tool_name") == "Write" and d.get("tool_use_id") == tool_id and
        d.get("input", {}).get("file_path") == forbidden for d in control_denials)
    honored = any(r.get("tool_use_id") == tool_id and r.get("is_error") and
        "Denied by XaYa read-only policy" in str(r.get("content", "")) for r in tool_results)
    if requested and honored:
        return True
    raise ValueError("No matching native permission denial for Write")


class OscTracker:
    def __init__(self):
        self.buffer = b""
        self.reports = []
        self.records = {}
        self.cycles = []
        self.active = None

    def feed(self, data, timestamp):
        self.buffer += data
        replies = []
        while True:
            start = self.buffer.find(b"\x1b]7501;")
            if start < 0:
                self.buffer = self.buffer[-8:]
                break
            self.buffer = self.buffer[start:]
            end = re.search(rb"\x07|\x1b\\", self.buffer)
            if end is None:
                if len(self.buffer) > 8192:
                    self.buffer = b""
                break
            payload = self.buffer[7:end.start()]
            self.buffer = self.buffer[end.end():]
            if payload == b"?":
                replies.append(SUPPORT_REPLY)
                continue
            try:
                fields = dict(field.split("=", 1) for field in payload.decode().split(":"))
            except (ValueError, UnicodeError):
                continue
            state = fields.get("state")
            if state not in ("idle", "working", "done", "blocked", "error", "clear"):
                continue
            self.reports.append({**fields, "received_at": timestamp})
            identifier = fields.get("id", "")
            if state == "clear":
                self.records = {key: value for key, value in self.records.items()
                    if identifier and key != identifier and not key.startswith(identifier + "/")}
            else:
                self.records[identifier] = state
            busy = any(state in ("working", "blocked") for state in self.records.values())
            if busy and self.active is None:
                self.active = timestamp
            elif not busy and self.active is not None:
                self.cycles.append({"start": self.active, "end": timestamp})
                self.active = None
        return replies


def match_boundaries(cycles, turns, tolerance=1.5):
    if isinstance(cycles, OscTracker):
        if cycles.active is not None:
            raise ValueError("OSC has an extra unfinished turn")
        cycles = cycles.cycles
    if not turns or len(cycles) != len(turns):
        raise ValueError(f"Boundary count mismatch: {len(cycles)} OSC / {len(turns)} native")
    result = []
    previous = float("-inf")
    for cycle, turn in zip(cycles, turns):
        if cycle["start"] < previous or cycle["end"] < cycle["start"]:
            raise ValueError("OSC cycles are not ordered")
        previous = cycle["end"]
        if abs(cycle["start"] - turn["start"]) > tolerance or abs(cycle["end"] - turn["end"]) > tolerance:
            raise ValueError(f"OSC/native boundary divergence: {turn['id']}")
        result.append({"turn_id": turn["id"], "start_skew_ms": round((cycle["start"] - turn["start"]) * 1000, 2),
                       "end_skew_ms": round((cycle["end"] - turn["end"]) * 1000, 2)})
    return result
