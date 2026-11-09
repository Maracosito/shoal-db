"""Parsing of tide gauge CSV exports into Reading records."""

from __future__ import annotations

import csv
from dataclasses import dataclass
from datetime import datetime
from typing import Iterable, Iterator


@dataclass(frozen=True)
class Reading:
    """A single water level observation from a tide gauge station."""

    timestamp: datetime
    level_m: float
    station: str = "unknown"


def parse_csv(lines: Iterable[str], station: str = "unknown") -> Iterator[Reading]:
    """Parse CSV rows with columns timestamp, level_m and optional station.

    Timestamps must be ISO-8601. Rows with a blank level are skipped rather
    than raising, since real gauge exports contain gaps.
    """
    reader = csv.DictReader(lines)
    for row in reader:
        raw_level = (row.get("level_m") or "").strip()
        if not raw_level:
            continue
        yield Reading(
            timestamp=datetime.fromisoformat(row["timestamp"].strip()),
            level_m=float(raw_level),
            station=(row.get("station") or station).strip() or station,
        )


def parse_file(path: str, station: str = "unknown") -> Iterator[Reading]:
    """Parse a CSV file of water level readings from disk."""
    with open(path, "r", encoding="utf-8", newline="") as fh:
        yield from parse_csv(fh, station=station)
