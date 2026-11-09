/**
 * Filter expression builders.
 *
 * Filters are plain JSON objects sent to the server unmodified, so they can
 * also be constructed by hand. These helpers exist to make construction
 * type-safe and readable:
 *
 * ```ts
 * import { and, eq, gte, isIn, not } from "@shoal-db/client";
 *
 * const f = and(
 *   eq("lang", "en"),
 *   gte("stars", 100),
 *   not(isIn("status", ["archived", "hidden"])),
 * );
 * ```
 *
 * The wire format mirrors the Shoal server's filter grammar
 * (see docs/api/filters in the main repository).
 */

/** Scalar values allowed in filter comparisons. */
export type Scalar = string | number | boolean | null;

export type ComparisonOp = "eq" | "neq" | "gt" | "gte" | "lt" | "lte";

export interface ComparisonFilter {
  op: ComparisonOp;
  field: string;
  value: Scalar;
}

export interface InFilter {
  op: "in";
  field: string;
  values: Scalar[];
}

export interface ContainsAnyFilter {
  op: "contains_any";
  field: string;
  values: Scalar[];
}

export interface PrefixFilter {
  op: "prefix";
  field: string;
  value: string;
}

export interface AndFilter {
  op: "and";
  filters: Filter[];
}

export interface OrFilter {
  op: "or";
  filters: Filter[];
}

export interface NotFilter {
  op: "not";
  filter: Filter;
}

/** Any filter expression accepted by the query endpoint. */
export type Filter =
  | ComparisonFilter
  | InFilter
  | ContainsAnyFilter
  | PrefixFilter
  | AndFilter
  | OrFilter
  | NotFilter;

/** `field == value` */
export function eq(field: string, value: Scalar): ComparisonFilter {
  return { op: "eq", field, value };
}

/** `field != value` */
export function neq(field: string, value: Scalar): ComparisonFilter {
  return { op: "neq", field, value };
}

/** `field > value` */
export function gt(field: string, value: Scalar): ComparisonFilter {
  return { op: "gt", field, value };
}

/** `field >= value` */
export function gte(field: string, value: Scalar): ComparisonFilter {
  return { op: "gte", field, value };
}

/** `field < value` */
export function lt(field: string, value: Scalar): ComparisonFilter {
  return { op: "lt", field, value };
}

/** `field <= value` */
export function lte(field: string, value: Scalar): ComparisonFilter {
  return { op: "lte", field, value };
}

/** `field IN (values...)` */
export function isIn(field: string, values: Scalar[]): InFilter {
  return { op: "in", field, values };
}

/**
 * `field` is an array attribute that contains at least one of `values`.
 */
export function containsAny(field: string, values: Scalar[]): ContainsAnyFilter {
  return { op: "contains_any", field, values };
}

/** String attribute starts with `value`. */
export function prefix(field: string, value: string): PrefixFilter {
  return { op: "prefix", field, value };
}

/** Logical conjunction. Requires at least one operand. */
export function and(...filters: Filter[]): AndFilter {
  if (filters.length === 0) {
    throw new Error("and() requires at least one filter");
  }
  return { op: "and", filters };
}

/** Logical disjunction. Requires at least one operand. */
export function or(...filters: Filter[]): OrFilter {
  if (filters.length === 0) {
    throw new Error("or() requires at least one filter");
  }
  return { op: "or", filters };
}

/** Logical negation. */
export function not(filter: Filter): NotFilter {
  return { op: "not", filter };
}
