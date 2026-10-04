# ADR-001 — Fork independence from trickle-labs/pg-ripple

- Status: accepted (2026-10-04, Gustavo).
- Scope: `agentxagi/pg-ripple` (this repository) is the canonical, independent
  line for the pg_ripple extension running ValorBrain production.

## Context

This repository started as a fork of `trickle-labs/pg-ripple`. The production
line (what `/opt/pg-ripple` builds and what ships as the installed `.so`) is
**this** fork's `main`. Measured 2026-10-04:

- The lines diverged on **2026-05-26** (merge-base `31d4a2c9`).
- Upstream shipped 23 commits since (v0.128.1 → v0.136.0, 2026-08-20 →
  2026-09-01, then quiet); our line shipped 46 (the 4-arg graph-scoped
  `justify` overload, SHACL write guard, dead-letter drain, datalog
  correctness fixes for our workloads, VAL-349/350/351/353 hardening).
- **The version line is double-spent**: both lines shipped releases numbered
  v0.128.1 → v0.136.0 with different content — the migration files
  `sql/pg_ripple--0.129.0--0.130.0.sql` and onward exist on both sides with
  diverging content. A test merge of upstream `main` produced **19 conflicts**
  (release metadata, the migration chain, `src/datalog/compiler/mod.rs`,
  pg_regress expectations).
- Upstream's post-split work of real value to us is narrow: the W3C SPARQL
  conformance fixes (`13aad42c`, touches `src/sparql/**` which every ValorBrain
  read path uses) — worth a targeted evaluation, not an absorption. The
  headline v0.129.0 "JSON writeback correctness" does not affect us: our write
  path never uses JSON writeback.
- Precedent: `pgturbohybrid` went independent on 2026-08-18 after a failed
  upstream-absorption attempt, and has been healthier for it.

## Decision

1. **`agentxagi/pg-ripple` `main` is the single source of truth** and evolves
   independently. No merges from upstream, no PRs to upstream.
2. **Remote topology** on working checkouts: `origin` = `agentxagi/pg-ripple`;
   `trickle-labs` may remain as a read-only remote named `upstream`, for
   evaluating selective cherry-picks. It is a reference, not a source.
3. **Selective cherry-pick policy**: upstream commits are candidates only when
   they fix a defect we can reproduce in our workloads. Each candidate is
   evaluated against our tree first (distance from the 2026-05-26 base may not
   apply cleanly) and goes through the normal PR + CI + owner-gated extension
   publish (VAL-201 rule: the production `.so` swap is a human decision).
4. **Version numbering is ours from here.** Because upstream double-spent
   v0.128.1–v0.136.0, our next version bump **jumps to `0.140.0`** (then
   continues) so a version number always names exactly one artifact lineage.
   The installed production build reports `0.130.0` from our line; the next
   release corrects the record.

## Consequences

- Cherry-picks become the only upstream mechanism — expect them to be rare and
  deliberate (the first candidate: W3C SPARQL conformance, tracked separately).
- The divergence cost is now explicit and bounded: no more surprise
  compare-page deltas; upstream movement never lands on `main` unreviewed.
- Anything upstream gains that we want must be ported and tested here — the
  maintenance price of independence, accepted in exchange for a version line
  and a trust model we control.
