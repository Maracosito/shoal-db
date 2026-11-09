"""Summary statistics over sequences of tide gauge readings.

Feature branch: adds least-squares harmonic fitting as the basis for tide
prediction (see forecast.py).
"""

from __future__ import annotations

import math
from typing import List, Sequence, Tuple

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
    """Simple trailing moving average used to smooth out wind chop."""
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


def harmonic_fit(
    times_hours: Sequence[float],
    levels: Sequence[float],
    period_hours: float = 12.42,
) -> Tuple[float, float, float]:
    """Least-squares fit of level(t) = a + b*cos(wt) + c*sin(wt).

    The default period is the principal lunar semidiurnal constituent (M2,
    12.42 h). Returns (a, b, c). Solves the 3x3 normal equations directly
    with Gaussian elimination — no numpy required.
    """
    if len(times_hours) != len(levels) or len(times_hours) < 3:
        raise ValueError("harmonic_fit needs >= 3 paired samples")
    omega = 2.0 * math.pi / period_hours
    rows = [[1.0, math.cos(omega * t), math.sin(omega * t)] for t in times_hours]
    # Normal equations: (X^T X) beta = X^T y
    ata = [[sum(r[i] * r[j] for r in rows) for j in range(3)] for i in range(3)]
    aty = [sum(r[i] * y for r, y in zip(rows, levels)) for i in range(3)]
    # Gaussian elimination with partial pivoting on the 3x3 system.
    m = [ata[i] + [aty[i]] for i in range(3)]
    for col in range(3):
        pivot = max(range(col, 3), key=lambda r: abs(m[r][col]))
        if abs(m[pivot][col]) < 1e-12:
            raise ValueError("degenerate harmonic_fit system")
        m[col], m[pivot] = m[pivot], m[col]
        for r in range(col + 1, 3):
            factor = m[r][col] / m[col][col]
            for c in range(col, 4):
                m[r][c] -= factor * m[col][c]
    beta = [0.0, 0.0, 0.0]
    for r in range(2, -1, -1):
        beta[r] = (m[r][3] - sum(m[r][c] * beta[c] for c in range(r + 1, 3))) / m[r][r]
    return beta[0], beta[1], beta[2]
