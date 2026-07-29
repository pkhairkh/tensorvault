# Problem Catalog — Index

> A structured catalog of the technical problems the instruction-first,
> memory-centric database engine must solve. Each problem is classified,
> motivated by the research, and linked to the relevant math and engineering
> context.
>
> **Status legend**: 🔴 open (no solution yet) · 🟡 partial (prototype exists) · 🟢 solved (in the codebase)

---

## How to read this catalog

The catalog is organized by architectural layer, following the inversion:

```
Instruction Sets → Memory Hierarchy → Protocols → Storage → Executor → Schema (last)
```

Each problem file covers one layer. Cross-cutting concerns (math, benchmarks,
open research questions) have their own files. The query syntax approach is
documented separately because it spans all layers.

## File index

| # | File | Layer | Problems | Status mix |
|---|------|-------|----------|-----------|
| 00 | `00-overview.md` | — | This index | — |
| 01 | `01-instruction-set-problems.md` | Instruction sets | 14 | 🔴9 🟡4 🟢1 |
| 02 | `02-memory-hierarchy-problems.md` | Memory hierarchy | 12 | 🔴7 🟡4 🟢1 |
| 03 | `03-storage-format-problems.md` | Storage format | 10 | 🔴6 🟡3 🟢1 |
| 04 | `04-protocol-problems.md` | Protocol boundaries | 8 | 🔴6 🟡2 🟢0 |
| 05 | `05-mathematical-problems.md` | Math foundations | 15 | 🔴12 🟡3 🟢0 |
| 06 | `06-query-syntax-approach.md` | Query language | 9 | 🔴6 🟡3 🟢0 |
| 07 | `07-execution-problems.md` | Executor | 11 | 🔴8 🟡3 🟢0 |
| 08 | `08-benchmarking-problems.md` | Benchmarks | 8 | 🔴7 🟡1 🟢0 |
| 09 | `09-open-research-questions.md` | Research | 12 | 🔴12 🟡0 🟢0 |
| 10 | `10-glossary.md` | Reference | — | — |

**Total: 99 open or partial problems across 10 files.**

## Problem classification

Every problem is tagged with:

- **Layer**: which architectural layer it belongs to
- **Status**: 🔴 open / 🟡 partial / 🟢 solved
- **Math**: which mathematical pillar(s) apply (I=info, II=spectral, III=prob, IV=opt, V=cat)
- **Effort**: S (< 1 month) / M (1–3 months) / L (3–6 months) / XL (6+ months)
- **Impact**: low / medium / high / critical
- **Dependencies**: which other problems must be solved first

## The five hardest problems (the "must solve" list)

1. **P-05-03** — Derive a closed-form cost model combining Kingman queueing latency with AVX-512 throughput (🔴, math, XL, critical)
2. **P-02-04** — Tier-aware region migration policy with competitive ratio guarantees (🔴, opt+prob, L, critical)
3. **P-04-02** — CXL 3.0 fabric integration for single-rack coherent commit (🔴, none, L, critical)
4. **P-06-04** — Compile `(ε, δ)` approximate SQL to sketch kernels with propagated confidence (🔴, prob, L, critical)
5. **P-09-01** — Is there a tight lower bound on energy-per-query for a given memory hierarchy? (🔴, info+prob, XL, research)

## Reading order for a new contributor

1. Start with `10-glossary.md` to learn the terminology.
2. Read `06-query-syntax-approach.md` to understand the SQL surface we're building toward.
3. Read `01-instruction-set-problems.md` and `02-memory-hierarchy-problems.md` — these are the foundation.
4. Skim `05-mathematical-problems.md` to see what mathematical machinery is available.
5. Pick a problem from the "must solve" list or from your area of expertise.

## Relationship to the research documents

| Problem file | Primary research source |
|-------------|------------------------|
| 01-instruction-set | `docs/cpu_energy_kb.md` |
| 02-memory-hierarchy | `docs/cpu_energy_kb.md`, `docs/instruction_first_architecture.md` |
| 03-storage-format | `docs/instruction_first_architecture.md`, `docs/research/info_theory_for_db.md` |
| 04-protocol | `docs/cpu_energy_kb.md`, `docs/research/category_theory_topology_db.md` |
| 05-mathematical | `docs/math_foundations.md`, `docs/research/*` |
| 06-query-syntax | `docs/research/probability_sketching_for_db.md`, `docs/research/optimization_theory_db.md` |
| 07-execution | `docs/research/optimization_theory_db.md`, `docs/research/spectral_db_research.md` |
| 08-benchmarking | `docs/tpcc_analysis.md`, `docs/tpcc_math.md` |
| 09-open-research | `docs/math_foundations.md`, all research docs |

---

*This catalog is a living document. As problems are solved, update their status from 🔴 to 🟡 to 🟢. As new problems are discovered, add them in the appropriate file.*
