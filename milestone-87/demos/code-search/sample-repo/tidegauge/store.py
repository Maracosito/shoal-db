"""JSON persistence for tide gauge datasets."""

from __future__ import annotations

import json
from datetime import datetime
from typing import List, Sequence

from .parser import Reading


def save_readings(path: str, readings: Sequence[Reading]) -> None:
    """Write readings to a JSON file with ISO-8601 timestamps."""
    payload = [
        {"timestamp": r.timestamp.isoformat(), "level_m": r.level_m, "station": r.station}
        for r in readings
    ]
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(payload, fh, indent=2)


def load_readings(path: str) -> List[Reading]:
    """Load readings previously written by save_readings."""
    with open(path, "r", encoding="utf-8") as fh:
        payload = json.load(fh)
    return [
        Reading(
            timestamp=datetime.fromisoformat(item["timestamp"]),
            level_m=float(item["level_m"]),
            station=item.get("station", "unknown"),
        )
        for item in payload
    ]
