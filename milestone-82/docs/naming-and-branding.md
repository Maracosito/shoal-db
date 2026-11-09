# Naming, Branding & Clean-Room Rationale

## The name: Gannet

A **gannet** is a large seabird famous for plunge-diving: it soars high, spots fish
deep below the surface, then dives at high speed to retrieve them. That is exactly
this system's operating model — lightweight, stateless compute that "dives" into
deep, cheap object storage to retrieve precisely what a query needs, then surfaces
with results.

The name was chosen because it is:

1. **Descriptive of the architecture** (plunge-dive retrieval from a deep store),
2. **Legally distinct** — it shares no words, morphemes, sounds, or visual identity
   with Turbopuffer or any other search/database product we are aware of. A trademark
   and package-registry search at the time of selection found no active database,
   search-engine, or developer-infrastructure product named "Gannet"
   (`gannet` crates.io / PyPI / npm scopes are claimed under this project's
   namespacing as `gannet-core`, `gannetdb`, `@gannetdb/*`),
3. **Short, pronounceable, and brandable**, with an obvious mascot that is *not* a
   pufferfish or any fish at all.

Binary/package naming conventions:

| Artifact | Name |
|----------|------|
| Rust crates | `gannet-core`, `gannet-server`, `gannet-cli` |
| Server binary | `gannetd` |
| CLI binary | `gannet` |
| Python SDK | `gannetdb` |
| TypeScript SDK | `@gannetdb/client` |
| Default API port | `8718` (unassigned in the IANA registry; mnemonic: "GA" → 7-18) |

## Clean-room policy

This project is **inspired by the publicly documented product category** of
object-storage-native search (durable state in S3-compatible storage, stateless
compute, SSD/memory cache hierarchy, namespaces, vector + full-text + hybrid search,
metadata filtering, namespace branching). Turbopuffer is the best-known commercial
example and is acknowledged as prior art in the category.

The following rules are binding for all contributors:

1. **No source code** from Turbopuffer or any proprietary system may be read,
   referenced, decompiled, or copied while contributing to Gannet. Turbopuffer does
   not publish its engine source; nobody on this project has had access to it.
2. **No documentation text** may be copied or closely paraphrased. Gannet's docs are
   written from scratch. Public *facts* about the category (e.g., "S3 PUTs cost
   money, so batch your WAL writes") are not protectable and may inform design.
3. **No branding assets**: no names, logos, color schemes, mascots, website layouts,
   marketing copy, or trade dress derived from any other product.
4. **Original API design.** Gannet's HTTP API was designed from its own data model
   (organizations → projects → namespaces → documents) with its own endpoint names,
   request/response shapes, filter syntax, and error model. Any resemblance to other
   APIs reflects industry-standard REST conventions (e.g., `POST /v1/.../query`),
   not copying.
5. **Original storage format.** The manifest/WAL/segment layout in
   [storage-format.md](storage-format.md) is an independent design. Where it uses
   well-known public techniques (LSM-style manifests, IVF vector indexes, BM25,
   CRC-framed logs), those techniques come from the open literature (e.g., the
   log-structured merge-tree, IVF/Faiss papers, Robertson & Zaragoza's BM25 work)
   and from open-source systems with permissive licenses whose *ideas* — not code —
   informed the design.
6. **Benchmark honesty.** We do not claim performance parity with any commercial
   system unless we publish a reproducible benchmark demonstrating it.

Contributions that appear to violate these rules will be rejected, and maintainers
may require a re-implementation by a contributor who has not seen the offending
material.

## Visual identity (v1)

Plain-text wordmark "Gannet" in a standard open-source license-compatible font
(Inter / system UI). Mascot and logo work is deferred; any future logo must be an
original commission depicting a gannet seabird, with no resemblance to other
database/search mascots.
