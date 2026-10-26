---
name: resolve_aarch64_adrp_add_pairs
kind: technique
tags: aarch64, adrp, address-resolution, disassembly
promoted-from: builtin (2026-10-21)
description: How to resolve AArch64 ADRP/ADD page+offset pairs into concrete
  addresses when a decompiler shows symbolic (&A0)[index]-style references
  instead of real runtime addresses.
---

# Generic Guide: Resolving `ADRP` / `ADD` Address Pairs on AArch64

When reverse engineering an AArch64 binary, decompilers may show references such as:

```c
(&A0)[index]
(&B0)[index]
```

or other symbolic/global-looking expressions without clearly showing the actual runtime addresses. To recover the concrete addresses, inspect the raw assembly around the relevant code by disassembling it.

A common AArch64 pattern for constructing an address is:

```asm
ADRP xN, <page-relative target>
ADD  xN, xN, #<page-offset>
```

Together, these instructions build a full address.

---

## 1. Identify the Address-Building Instructions

Look for an `ADRP` followed by an `ADD` using the same destination register.

Example:

```asm
0xADDR1: ADRP x23, page +0xPAGE_DELTA
0xADDR2: ADD  x23, x23, #0xOFFSET

0xADDR3: ADRP x24, page +0xPAGE_DELTA
0xADDR4: ADD  x24, x24, #0xOFFSET
```

The `ADRP` loads a page-aligned base address into the register.

The `ADD` then adds the offset within that page.

---

## 2. Determine the Page Containing the `ADRP` Instruction

`ADRP` is PC-relative, but it uses the page containing the address of the `ADRP` instruction.

Pages are usually aligned to `0x1000` bytes.

To get the page of the instruction, clear the lower 12 bits of the `ADRP` instruction address.

For example, if the instruction is located at:

```text
0x4000123c
```

then the page is:

```text
0x40001000
```

because:

```text
0x4000123c & ~0xfff = 0x40001000
```

---

## 3. Add the `ADRP` Page Delta

If the disassembler shows:

```asm
ADRP x23, page +0x2a000
```

then add that page-relative value to the instruction page:

```text
base_page + 0x2a000
```

Using the example:

```text
0x40001000 + 0x2a000 = 0x4002b000
```

---

## 4. Add the Final `ADD` Offset

The following `ADD` instruction gives the offset within the page:

```asm
ADD x23, x23, #0x3b0
```

So the final address is:

```text
0x4002b000 + 0x3b0 = 0x4002b3b0
```

---

## General Formula

For an instruction pair like:

```asm
ADRP xN, page + PAGE_DELTA
ADD  xN, xN, #OFFSET
```

where the `ADRP` instruction is located at address `ADRP_ADDR`, the resolved address is:

```text
page(ADRP_ADDR) + PAGE_DELTA + OFFSET
```

where:

```text
page(ADRP_ADDR) = ADRP_ADDR & ~0xfff
```

So:

```text
resolved_address = (ADRP_ADDR & ~0xfff) + PAGE_DELTA + OFFSET
```

---

## Example

Given:

```asm
0x4000123c: ADRP x23, page +0x2a000
0x40001240: ADD  x23, x23, #0x3b0

0x40001244: ADRP x24, page +0x2a000
0x40001248: ADD  x24, x24, #0x3c8
```

First compute the page of the `ADRP` instruction:

```text
0x4000123c & ~0xfff = 0x40001000
```

Then resolve the first address:

```text
0x40001000 + 0x2a000 + 0x3b0 = 0x4002b3b0
```

Resolve the second address:

```text
0x40001000 + 0x2a000 + 0x3c8 = 0x4002b3c8
```

So the two referenced addresses are:

```text
A0 = 0x4002b3b0
B0 = 0x4002b3c8
```

---

## Notes

- `ADRP` works at page granularity, typically `0x1000` bytes.
- The lower 12 bits of the `ADRP` instruction address are discarded.
- The following `ADD` usually supplies the offset within that page.
- This pattern is common for references to globals, string literals, static arrays, jump tables, and other data stored elsewhere in the binary.
- Always use the address of the `ADRP` instruction itself when calculating the page base.

