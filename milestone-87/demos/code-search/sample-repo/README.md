# tidegauge

A tiny library for working with tide gauge data: parse CSV exports of water
level readings, compute summary statistics, and persist datasets to JSON.

This project exists as a realistic-but-small corpus for the Lagoon
code-search demo. It is fully functional Python with no dependencies.

```python
from tidegauge import parse_file, mean_level, tidal_range

readings = list(parse_file("harbor.csv"))
print(mean_level(readings), tidal_range(readings))
```

CSV format: a header row of `timestamp,level_m[,station]` followed by ISO-8601
timestamps and water levels in metres.
