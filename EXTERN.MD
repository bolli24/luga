# Extern ABI Notes

These notes describe how Luga should lower arguments for external C functions on x86_64 Windows and Linux when using Cranelift.

Reference: System V AMD64 ABI Draft 0.99.6, section 3.2.3, Parameter Passing: https://refspecs.linuxbase.org/elf/x86_64-abi-0.99.pdf

## Cranelift Responsibilities

Cranelift handles physical argument placement once the signature is correct.

For normal ABI parameters such as `I32`, `I64`, `F32`, `F64`, or pointer values, Luga should emit one `AbiParam` per ABI-level argument and pass one Cranelift `Value` per `AbiParam`. 
Cranelift then decides whether each value goes in a register or on the stack.

Examples:

```text
Windows x64: RCX, RDX, R8, R9, then stack
Linux SysV:  RDI, RSI, RDX, RCX, R8, R9 for integer/pointer args, then stack
Linux SysV:  XMM0..XMM7 for float args, then stack
```

Cranelift does not classify C structs for the frontend. Luga must decide whether a struct argument becomes one value, multiple values, or an indirect/byval stack argument.

## Scalar Arguments

Integers, floats, bools, pointers, strings, and references are passed as normal Cranelift values with matching `AbiParam::new(...)` entries.

```text
i32     -> AbiParam::new(I32), pass I32 value
i64     -> AbiParam::new(I64), pass I64 value
f32     -> AbiParam::new(F32), pass F32 value
f64     -> AbiParam::new(F64), pass F64 value
string  -> AbiParam::new(pointer_type), pass pointer value
ref T   -> AbiParam::new(pointer_type), pass pointer value
```

## StructArgument In Cranelift

`ArgumentPurpose::StructArgument(size)` is for stack-passed by-value structs.

It means the CLIF-level value is a pointer to struct memory. The x64 backend copies `size` bytes from that pointer into the outgoing stack argument area.

Use it like this:

```rust
AbiParam::special(pointer_type, ArgumentPurpose::StructArgument(size))
```

Do not use `StructArgument` for small packed struct values such as raylib `Color`.

Incorrect:

```rust
AbiParam::special(types::I64, ArgumentPurpose::StructArgument(8))
```

Correct for a 4-byte by-value struct passed directly:

```rust
AbiParam::new(types::I32)
```

and pass an `I32` loaded from the struct storage.

## Windows x64 Struct Arguments

Windows x64 does not split struct arguments across multiple registers.

Classify each by-value struct argument by size:

```text
size 1 byte  -> pass direct as I8
size 2 bytes -> pass direct as I16
size 4 bytes -> pass direct as I32
size 8 bytes -> pass direct as I64
other sizes  -> pass indirect pointer
```

For direct struct arguments, load the corresponding integer value from the struct's storage and pass that value. Do not pass the struct address.

For indirect struct arguments, pass a pointer to suitable backing storage/copy using `AbiParam::new(pointer_type)`.

Cranelift handles whether direct values or pointers go in `RCX`, `RDX`, `R8`, `R9`, or on the stack.

Examples:

```text
struct Color { u8 r; u8 g; u8 b; u8 a; }
size 4 -> load I32 -> pass I32

struct Big12 { ... }
size 12 -> pass pointer
```

## Linux x86_64 SysV Struct Arguments

Linux x86_64 SysV classifies aggregate arguments by 8-byte chunks, called eightbytes.

Luga should classify by-value structs before building the Cranelift signature.

Basic rules:

```text
unaligned non-ZST field -> stack/byval
size > 32 bytes         -> stack/byval
size <= 8 bytes         -> one ABI value if classifiable
size 9..=16 bytes       -> one or two ABI values if classifiable
size 17..=32 bytes      -> stack/byval unless it is a vector/SSEUP aggregate
otherwise               -> stack/byval
```

The SysV reference initially allows aggregates up to four eightbytes. After field classes are merged, it applies a cleanup rule: if an aggregate is larger than two eightbytes, it is passed in memory unless the first eightbyte is `SSE` and every other eightbyte is `SSEUP`. For Luga structs without vector types such as `__m256`, this means ordinary C structs larger than 16 bytes are stack/byval.

Common direct cases:

```text
integer-only struct size 1      -> I8
integer-only struct size 2      -> I16
integer-only struct size 3..=4  -> I32
integer-only struct size 5..=8  -> I64
integer-only struct size 9..=16 -> two integer values, but not always I64 + I64
single f32 field                -> F32
single f64 field                -> F64
float-only aggregate            -> SSE-class values where applicable
mixed aggregate                 -> may split into integer and float ABI values
```

For 9..=16 byte integer-class structs, classify each eightbyte separately. The first eightbyte is usually `I64`; the second uses the smallest integer register type that covers the remaining bytes.

```text
total size 9  -> I64 + I8
total size 10 -> I64 + I16
total size 11 -> I64 + I32
total size 12 -> I64 + I32
total size 13 -> I64 + I64
total size 14 -> I64 + I64
total size 15 -> I64 + I64
total size 16 -> I64 + I64
```

Normal C layout often pads structs to their alignment, so `I64 + I64` is common, but it is not universal.

If different field classes occur in the same eightbyte, the classes are unified. Integer wins over SSE in the same eightbyte.

After classifying all eightbytes, if any eightbyte becomes `MEMORY`, the whole aggregate is passed in memory.

Example:

```c
struct S {
    float f;
    int i;
};
```

Layout:

```text
offset 0: f32
offset 4: i32
size 8
```

On Linux x86_64 SysV, both fields are in the same eightbyte. The `f32` contributes SSE and the `i32` contributes integer, so the eightbyte becomes integer-class. Pass it as:

```text
AbiParam::new(I64)
```

and load 8 bytes from the struct storage.

On Windows x64, the same struct is size 8, so it is also passed as one `I64`.

Contrast:

```c
struct T {
    double f;
    int i;
};
```

On Linux x86_64 SysV, this is typically size 16 and can be split into two ABI values, e.g. `F64` for the first eightbyte and an integer value for the second. On Windows x64, size 16 is not a direct struct size, so it is passed indirectly by pointer.

For direct/cast structs, load the ABI value or values from the struct storage at the ABI-chosen offsets and pass them as normal `AbiParam::new(...)` parameters.

For stack/byval structs, use:

```rust
AbiParam::special(pointer_type, ArgumentPurpose::StructArgument(size_aligned_to_8))
```

and pass a pointer to the struct storage/copy.

## Linux SysV Register Exhaustion

Cranelift handles stack placement for normal ABI params, but it does not know when multiple ABI params came from one source-level struct.

This matters for multi-slot aggregates. SysV requires a register-passed aggregate to fit completely in the remaining register classes. If it cannot fit, the whole aggregate is passed on the stack.

Linux SysV argument registers:

```text
integer/pointer: 6 registers
float/SSE:       8 registers
```

For scalar args, Cranelift can handle overflow to stack.

For aggregate args that expand to multiple ABI values, Luga must track remaining integer/SSE registers while classifying extern signatures. If the aggregate needs more registers than remain, classify the whole aggregate as stack/byval instead of splitting it.

Example:

```c
struct Pair { uint64_t a; uint64_t b; };
void f(uint64_t a, uint64_t b, uint64_t c, uint64_t d, uint64_t e, Pair p);
```

Only one integer register remains before `p`, but `Pair` needs two. SysV passes the entire `Pair` on the stack. Representing it as two independent `I64` params could incorrectly put one half in a register and one half on the stack.

For single-slot aggregates such as `Color`, Cranelift's normal overflow behavior is sufficient because the aggregate cannot be split.

## Raylib Color

raylib defines `Color` as:

```c
typedef struct Color {
    unsigned char r;
    unsigned char g;
    unsigned char b;
    unsigned char a;
} Color;
```

`Color` is 4 bytes.

For both Windows x64 and Linux x86_64 SysV:

```text
Color -> load I32 from struct storage -> pass I32
```

On little-endian machines, the packed value is:

```text
r | (g << 8) | (b << 16) | (a << 24)
```

For `Color { r: 74, g: 255, b: 122, a: 255 }`:

```text
0xff7aff4a
```

Do not pass a pointer for `Color`, and do not use `StructArgument` for it.
