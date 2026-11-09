"""Command line interface: summarise a tide gauge CSV export."""

from __future__ import annotations

import argparse
import sys

from .parser import parse_file
from .stats import find_high_tides, mean_level, tidal_range


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="tidegauge", description="Tide gauge data tools")
    parser.add_argument("csv", help="path to a CSV export of water level readings")
    parser.add_argument("--station", default="unknown", help="station name fallback")
    parser.add_argument(
        "--high-tide-threshold",
        type=float,
        default=None,
        help="if set, list local maxima at or above this level in metres",
    )
    return parser


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    readings = list(parse_file(args.csv, station=args.station))
    if not readings:
        print("no readings found", file=sys.stderr)
        return 1
    print(f"readings:    {len(readings)}")
    print(f"mean level:  {mean_level(readings):.3f} m")
    print(f"tidal range: {tidal_range(readings):.3f} m")
    if args.high_tide_threshold is not None:
        for high in find_high_tides(readings, args.high_tide_threshold):
            print(f"high tide:   {high.timestamp.isoformat()}  {high.level_m:.3f} m")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
