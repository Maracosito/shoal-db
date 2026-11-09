import { describe, expect, it } from "vitest";
import {
  and,
  containsAny,
  eq,
  gt,
  gte,
  isIn,
  lt,
  lte,
  neq,
  not,
  or,
  prefix,
} from "../src/filters.js";

describe("filter builders", () => {
  it("builds comparison filters", () => {
    expect(eq("lang", "en")).toEqual({ op: "eq", field: "lang", value: "en" });
    expect(neq("lang", "en")).toEqual({ op: "neq", field: "lang", value: "en" });
    expect(gt("stars", 10)).toEqual({ op: "gt", field: "stars", value: 10 });
    expect(gte("stars", 10)).toEqual({ op: "gte", field: "stars", value: 10 });
    expect(lt("stars", 10)).toEqual({ op: "lt", field: "stars", value: 10 });
    expect(lte("stars", 10)).toEqual({ op: "lte", field: "stars", value: 10 });
  });

  it("builds set filters", () => {
    expect(isIn("status", ["a", "b"])).toEqual({
      op: "in",
      field: "status",
      values: ["a", "b"],
    });
    expect(containsAny("tags", ["rust", "db"])).toEqual({
      op: "contains_any",
      field: "tags",
      values: ["rust", "db"],
    });
  });

  it("builds prefix filters", () => {
    expect(prefix("path", "src/")).toEqual({
      op: "prefix",
      field: "path",
      value: "src/",
    });
  });

  it("builds nested boolean expressions", () => {
    const f = and(eq("lang", "en"), or(gte("stars", 100), not(eq("archived", true))));
    expect(f).toEqual({
      op: "and",
      filters: [
        { op: "eq", field: "lang", value: "en" },
        {
          op: "or",
          filters: [
            { op: "gte", field: "stars", value: 100 },
            { op: "not", filter: { op: "eq", field: "archived", value: true } },
          ],
        },
      ],
    });
  });

  it("rejects empty boolean operands", () => {
    expect(() => and()).toThrow();
    expect(() => or()).toThrow();
  });

  it("filters serialize cleanly to JSON", () => {
    const f = and(eq("a", 1), isIn("b", [null, "x"]));
    expect(JSON.parse(JSON.stringify(f))).toEqual(f);
  });
});
