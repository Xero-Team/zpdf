# Copy allocation-callsite diagnostic

The original 575-pair macOS copy_heap regression remains recorded. Installed
allocator inspection already shows that ordinary malloc derives a fallback
type from its caller PC, then combines that type with executable-specific
keys. Two unchanged logical allocation sites can therefore receive different
relative placement after an otherwise unrelated binary change.

This separate diagnostic tests a prospective measurement control without
changing production code, original workloads, allocator state or acceptance
data. Copy the unchanged 21,088-byte font fixture through a non-inlined helper.
In shared-callsite mode, allocate both the source buffer and each timed
destination through that helper. In distinct-callsite mode, allocate the
source through another non-inlined helper and use the same timed destination
helper. Inspect the generated assembly before interpreting these labels.

Compile two optimized probe binaries from the same Rust source, using
equal-length printed build labels and executable names. Retain executable/source hashes and
disassembly. The labels deliberately create different executable identities;
they are not two zpdf implementations. The same actual source bytes and timed
copy helper apply to both. Assert copied bytes outside timing.

Run 21 fixed blocks, each containing two launches of both modes in both
binaries: 168 processes and 1,680 raw samples. Each process warms for at least
0.1 s, then records ten samples of 131,072 allocation/copy/drop iterations.
Rotate and reverse the four condition orders and alternate repetition order.
Use default scheduling. Retain failed launches and stop on invalid output;
do not stop early or extend based on intermediate outcomes.

Report same-binary paired 95% intervals for each of the four conditions, using
the existing two-sided +/-5% calibration rule. Also report the paired effect
of binary identity within each mode; those contrasts are descriptive and are
not production acceptance. Every observation remains visible. Source and
original font-benchmark hashes must remain unchanged across the experiment.

A passing probe would justify further investigation of a controlled source
allocation site. It would not clear the original regression or validate a
modified production benchmark without fresh matched before/after evidence.
