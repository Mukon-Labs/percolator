# Verification automation

The `Kani verification` workflow pins Kani to the version declared in
`kani-list.json` and separates fast pull-request feedback from expensive proof
layers.

- Every pull request and push runs the Rust tests and
  `proof_v16_floor_div_signed_conservative_matches_small_reference` as a smoke
  proof.
- The weekly schedule runs the complete plain proof layer.
- A manual dispatch can run the plain, inductive-closure, or function-contract
  layer. The `smoke` choice repeats only the normal fast gate.

The full jobs are bounded by GitHub's six-hour job timeout. A timeout is a
failed verification run, not an approval. It should be investigated or split
into deterministic shards before treating that layer as green.

Equivalent local commands are:

```sh
cargo test --locked
cargo kani --tests --features fuzz --harness proof_v16_floor_div_signed_conservative_matches_small_reference
cargo kani --tests --features fuzz
cargo kani --tests --features fuzz,closure
cargo kani --tests --features fuzz,contracts -Z function-contracts
```
