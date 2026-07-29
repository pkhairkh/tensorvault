# Architecture

> Design documents for the instruction-first, memory-centric database engine.

## Documents

| Document | Lines | What it covers |
|----------|-------|----------------|
| **[instruction-first.md](./instruction-first.md)** | ~470 | The design philosophy: 3 invariants, storage format, kernel table, executor, protocol boundaries |
| **[cpu-energy-kb.md](./cpu-energy-kb.md)** | ~840 | Per-instruction energy, latency, throughput for modern x86 (Ice Lake → Zen 5). The engineering reference for kernel design. |

## How these relate

`instruction-first.md` is the **what and why** — the architecture.  
`cpu-energy-kb.md` is the **how much** — the numbers that drive kernel selection.

Every kernel in the kernel table (`src/kernel/`) is justified by numbers in
`cpu-energy-kb.md`. Every design decision in `instruction-first.md` traces
back to a row in the energy knowledgebase.
