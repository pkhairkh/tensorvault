# Documentation Index

> Entry point for all TensorVault documentation. Read in order if you're new;
> jump to a section if you're looking for something specific.

## Start here

| Document | What it is | When to read |
|----------|-----------|--------------|
| **[FINE_DRAFT.md](./FINE_DRAFT.md)** | The master document: the venture, the architecture, the problem catalog with solutions, the build plan | **Read first.** This is the comprehensive fine draft. |
| **[../README.md](../README.md)** | Project overview, quick start, repository layout | If you just want to run the code |
| **[../ARCHITECTURE.md](../ARCHITECTURE.md)** | The instruction-first architecture in 1 page | If you want the design summary |

## Directory structure

```
docs/
├── FINE_DRAFT.md              ← THE master document (start here)
├── architecture/              ← Design docs: the architecture + CPU energy knowledgebase
├── research/                  ← Mathematical foundations + 5 domain deep-dives + 5 wave evaluations
│   ├── math-foundations.md    ← Synthesis of all 5 mathematical pillars
│   ├── math-enhancements.md   ← 5 concrete enhancement proposals
│   ├── domains/               ← Deep research per mathematical domain
│   └── waves/                 ← Per-problem solution evaluation (performance/time/energy)
├── problems/                  ← Problem catalog: 99 problems across 10 files
├── benchmarks/                ← TPC-C and TPC-H analysis
└── archive/                   ← Old NaN-boxing thesis (superseded)
```

## Reading order for a new contributor

1. **[FINE_DRAFT.md](./FINE_DRAFT.md)** — the whole venture in one document
2. **[architecture/instruction-first.md](./architecture/instruction-first.md)** — the design philosophy
3. **[problems/README.md](./problems/README.md)** — the problem catalog index
4. **[research/math-foundations.md](./research/math-foundations.md)** — the mathematical grounding
5. Pick a problem from the catalog and dive into its wave evaluation

## Reading order for a researcher

1. **[research/math-foundations.md](./research/math-foundations.md)** — the 5-pillar synthesis
2. **[research/domains/](./research/domains/)** — pick the domain closest to your expertise
3. **[research/waves/](./research/waves/)** — see the per-problem solution evaluations
4. **[problems/09-open-research.md](./problems/09-open-research.md)** — the 12 PhD-thesis-scale open questions

## Reading order for an engineer

1. **[../ARCHITECTURE.md](../ARCHITECTURE.md)** — the design summary
2. **[architecture/cpu-energy-kb.md](./architecture/cpu-energy-kb.md)** — the per-instruction energy reference
3. **[problems/](./problems/)** — pick a problem tagged 🟡 (partial) or 🔴 (open)
4. **[research/waves/](./research/waves/)** — see the candidate solutions with effort estimates
