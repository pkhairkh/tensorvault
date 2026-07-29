# Archive

> Documents from the earlier NaN-boxing thesis, superseded by the current
> instruction-first, memory-centric architecture. Kept for historical
> reference.

## What's here

| Document | What it was | Why superseded |
|----------|------------|----------------|
| [position-paper.{tex,pdf}](./position-paper.tex) | 8-page SIGMOD/VLDB vision-track paper on "Bit-Uniform Relational Storage" | The NaN-boxing thesis was replaced by the instruction-first architecture. The bit-uniform idea survives as a storage-format detail, not the load-bearing invariant. |
| [mdl-sketch.{tex,pdf}](./mdl-sketch.tex) | Formal MDL schema-selection algorithm with theorems and proofs | The MDL approach is still used (see `../research/math-enhancements.md` Enhancement 4), but the formal category-theory framing was dropped. |
| [commodity-hw.{tex,pdf}](./commodity-hw.tex) | AVX-512-only execution path design doc | Folded into `../architecture/instruction-first.md` and the kernel table implementation. |

## Current status

The ideas from these documents are not abandoned — they're absorbed into the
current architecture:

- **NaN-boxing** → the 64-bit word storage format (but not the "niche-filled
  tagged union" thesis)
- **MDL schema selection** → `src/schema/mdl.rs` (implemented)
- **AVX-512 kernels** → `src/kernel/` (implemented, 16 kernels)

The current architecture is documented in:
- [`../FINE_DRAFT.md`](../FINE_DRAFT.md) — the master document
- [`../architecture/instruction-first.md`](../architecture/instruction-first.md) — the design
- [`../research/math-foundations.md`](../research/math-foundations.md) — the math
