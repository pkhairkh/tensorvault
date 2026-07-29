# Research

> Mathematical foundations, domain deep-dives, and per-problem solution
> evaluations. This is the theoretical backbone of the engine.

## Documents

### Syntheses

| Document | Lines | What it covers |
|----------|-------|----------------|
| **[math-foundations.md](./math-foundations.md)** | ~390 | Synthesis of 5 mathematical pillars into a unified architecture. 50 techniques with formulas and DB applications. |
| **[math-enhancements.md](./math-enhancements.md)** | ~480 | 5 concrete enhancement proposals, ranked by impact ÷ effort. Each with math, implementation sketch, expected win. |

### Domain deep-dives ([domains/](./domains/))

Each file is a standalone research pass across one mathematical domain,
with formal definitions, theorems, and citations.

| Document | Domain | Key techniques |
|----------|--------|---------------|
| [information-theory.md](./domains/information-theory.md) | Info theory | Rate-distortion, ANS, ECC, Kolmogorov, quantization, AGM bound |
| [spectral-graph-theory.md](./domains/spectral-graph-theory.md) | Spectral | Cheeger, JL, randomized SVD, tensor train, spectral sparsification |
| [probability-and-sketching.md](./domains/probability-and-sketching.md) | Probability | Hoeffding, McDiarmid, HLL, Count-Min, LSH, Kingman, PAC |
| [optimization-theory.md](./domains/optimization-theory.md) | Optimization | LP, Selinger DP, B&B, Lagrangian, MWU, submodular, MDP, SDP |
| [category-theory.md](./domains/category-theory.md) | Category theory | Functorial migration, topos, linear types, sheaves, univalence |

### Per-problem solution evaluations ([waves/](./waves/))

Each wave evaluates 12–26 problems from the [problem catalog](../problems/)
against scientific literature, proposing 2–3 candidate solutions per problem
with performance/time/energy trade-offs.

| Wave | Problems | Domain |
|------|----------|--------|
| [01-instruction-memory.md](./waves/01-instruction-memory.md) | 26 | Instruction sets (14) + Memory hierarchy (12) |
| [02-storage-protocol.md](./waves/02-storage-protocol.md) | 18 | Storage (10) + Protocol (8) |
| [03-math-query-syntax.md](./waves/03-math-query-syntax.md) | 24 | Math (15) + Query syntax (9) |
| [04-execution-benchmarking.md](./waves/04-execution-benchmarking.md) | 19 | Execution (11) + Benchmarking (8) |
| [05-open-research.md](./waves/05-open-research.md) | 12 | PhD-thesis-scale open questions |

## How to use this directory

1. Start with `math-foundations.md` for the big picture.
2. Dive into `domains/` for the math behind a specific technique.
3. Check `waves/` for concrete solution proposals with effort estimates.
4. Cross-reference with `../problems/` for the problem definitions.
