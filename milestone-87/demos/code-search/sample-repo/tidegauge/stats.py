"""Summary statistics over sequences of tide gauge readings."""

from __future__ import annotations

from typing import List, Sequence

from .parser import Reading


def mean_level(readings: Sequence[Reading]) -> float:
    """Arithmetic mean water level in metres. Raises on empty input."""
    if not readings:
        raise ValueError("mean_level requires at least one reading")
    return sum(r.level_m for r in readings) / len(readings)


def tidal_range(readings: Sequence[Reading]) -> float:
    """Difference between the highest and lowest observed level."""
    if not readings:
        raise ValueError("tidal_range requires at least one reading")
    levels = [r.level_m for r in readings]
    return max(levels) - min(levels)


def moving_average(levels: Sequence[float], window: int) -> List[float]:
    """Simple trailing moving average used to smooth out wind chop.

    Returns a list the same length as the input; the first window-1 entries
    average over the partial prefix.
    """
    if window < 1:
        raise ValueError("window must be >= 1")
    out: List[float] = []
    running = 0.0
    for i, level in enumerate(levels):
        running += level
        if i >= window:
            running -= levels[i - window]
        out.append(running / min(i + 1, window))
    return out


def find_high_tides(readings: Sequence[Reading], threshold: float) -> List[Reading]:
    """Return local maxima above a threshold — candidate high tide events."""
    highs: List[Reading] = []
    for i in range(1, len(readings) - 1):
        prev_r, cur, next_r = readings[i - 1], readings[i], readings[i + 1]
        if cur.level_m >= threshold and cur.level_m >= prev_r.level_m and cur.level_m > next_r.level_m:
            highs.append(cur)
    return highs
