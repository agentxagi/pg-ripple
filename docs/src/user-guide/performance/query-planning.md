# Query Planning

pg_ripple translates SPARQL algebra into PostgreSQL SQL before execution.
This page describes how the plan is constructed and how to tune it.

## Plan cache

Every translated plan is cached per-backend in an LRU cache keyed on an
**algebra digest** (XXH3-128 of the normalised SPARQL IR, plus the current
values of `max_path_depth` and `bgp_reorder`).  This means:

- Whitespace variants and prefix-alias variants of the same query share one
  cache slot.
- Changing `SET pg_ripple.bgp_reorder = off` causes a cache miss and triggers
  re-translation.
- The cache is backend-local and cleared on connection close.

Inspect cache health with:

```sql
SELECT * FROM pg_ripple.plan_cache_stats();
-- Returns: (hit_count, miss_count, current_size, capacity)
```

Reset counters without a reconnect:

```sql
SELECT pg_ripple.reset_plan_cache();
```

## BGP reordering

By default (`pg_ripple.bgp_reorder = on`) the SPARQL optimizer permutes triple
patterns in a BGP to minimise intermediate result sizes based on per-predicate
triple counts.  Disable to force left-to-right evaluation order:

```sql
SET pg_ripple.bgp_reorder = off;
```

## Predicate catalog

The predicate catalog (`storage/catalog.rs`) caches predicate → VP table OID
mappings to eliminate one SPI lookup per predicate per query.  For a 10-atom
BGP this reduces dictionary-related SPI overhead from 10 to 1.

```sql
-- Invalidate after schema changes or shape updates:
SELECT pg_ripple.invalidate_catalog_cache();
```

The catalog cache is enabled by default.  Disable for debugging:

```sql
SET pg_ripple.predicate_cache_enabled = off;
```

## SHACL-driven SQL hints

After loading SHACL shapes, the SQL generator reads per-predicate hints from
`_pg_ripple.shape_hints` to produce more efficient SQL:

| SHACL constraint | SQL optimisation |
|---|---|
| `sh:maxCount 1` | Omit `DISTINCT` — at most one binding per subject |
| `sh:minCount 1` | Use `INNER JOIN` instead of `LEFT JOIN` for OPTIONAL |

Load shapes and verify hints were written:

```sql
SELECT pg_ripple.load_shacl($$ @prefix sh: <...> . @prefix ex: <...> . ex:MyShape a sh:NodeShape ; sh:targetClass ex:Thing ; sh:property [ sh:path ex:name ; sh:maxCount 1 ] . $$);

SELECT * FROM _pg_ripple.shape_hints LIMIT 10;
```

## EXPLAIN a SPARQL query

Use `sparql_explain` to see the SQL generated for a query:

```sql
SELECT pg_ripple.sparql_explain(
  'SELECT * WHERE { ?s <http://schema.org/name> ?name }',
  analyze := false
);
```

Pass `analyze := true` to include actual runtime statistics (runs the query).

## Property path depth

Recursive property paths (`*`, `+`) use `WITH RECURSIVE` with a set-semantics
`UNION` (no `CYCLE` clause, which enumerates every simple path) and a
configurable depth limit:

```sql
SET pg_ripple.max_path_depth = 32;   -- default: 100; 0 = unlimited
```

The limit has a cost. With `max_path_depth > 0` the recursion carries the hop
count, so a pair reachable by walks of many lengths is kept once per length.
On a symmetric, tree-shaped predicate every pair comes back at each depth of the
same parity: a binary tree of 511 nodes (1,020 edges, 261,121 pairs), unanchored
`?s <rel>+ ?o`, produced 11.7 million CTE rows and took 20.5 s with the default
100, against 261 thousand rows and 2.0 s with `0`. The limit is still the only
cap on long chains, and the recursive `UNION` keeps its hash table in memory, so
keep it on shared hosts; anchoring the path (a bound subject or object) keeps the
query small (76 ms for the same tree from one node).

> **Note:** `property_path_max_depth` is a deprecated alias for `max_path_depth`
> and will be removed in a future release.

## Useful GUCs for query planning

| GUC | Default | Effect |
|-----|---------|--------|
| `pg_ripple.bgp_reorder` | `on` | Reorder triple patterns by selectivity |
| `pg_ripple.max_path_depth` | `64` | Max recursion depth for `*` / `+` paths |
| `pg_ripple.predicate_cache_enabled` | `on` | Cache predicate → VP table OIDs |
| `pg_ripple.plan_cache_size` | `256` | LRU capacity of the per-backend plan cache |
