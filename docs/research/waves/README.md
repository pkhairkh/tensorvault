# Wave Research — Per-Problem Solution Evaluations

> 5 waves of subagent-based research, each evaluating 12–26 problems from
> the [problem catalog](../../problems/) against scientific literature.
> Every problem gets 2–3 candidate solutions, each evaluated on 3 axes:
> **performance**, **time-to-implement**, **energy cost**.

## The 5 waves

| Wave | File | Problems | Coverage |
|------|------|----------|----------|
| 01 | [01-instruction-memory.md](./01-instruction-memory.md) | 26 | Instruction sets (14) + Memory hierarchy (12) |
| 02 | [02-storage-protocol.md](./02-storage-protocol.md) | 18 | Storage format (10) + Protocol (8) |
| 03 | [03-math-query-syntax.md](./03-math-query-syntax.md) | 24 | Mathematical (15) + Query syntax (9) |
| 04 | [04-execution-benchmarking.md](./04-execution-benchmarking.md) | 19 | Execution (11) + Benchmarking (8) |
| 05 | [05-open-research.md](./05-open-research.md) | 12 | PhD-thesis-scale open questions |

## The 3-axis evaluation format

Every solution is evaluated on:

| Axis | What it measures | Example |
|------|-----------------|---------|
| **Performance** | Throughput/latency with citation | 19 G cells/sec (VPCMPEQQ, Polychroniou VLDB 2015) |
| **Time** | Engineering months with rationale | 2–3 months (kernel table + CPUID + dispatch) |
| **Energy** | Joules per operation with citation | ~0.5 nJ/cell (L3 hit + ALU, Kim et al. MEMSYS 2015) |

## How to use these evaluations

1. Find your problem in [../../problems/](../../problems/)
2. Look up its wave evaluation here
3. Compare the candidate solutions on the 3 axes
4. Check the "Recommendation" section for the preferred approach
5. Read the cited papers for details

## Cross-wave synthesis

The [../../FINE_DRAFT.md](../../FINE_DRAFT.md) synthesizes all 5 waves into
a single build plan with a critical path, time budget, and energy budget.
