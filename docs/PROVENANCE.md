# Source provenance

This repository is a maintained-fork distribution. It preserves the identity
of the source it started from and records later changes separately.

## Extraction checkpoint

The initial standalone source was copied from TheKernel commit
`dbbaea9ff0ee6c63bdfb9d9828d4a8d25ba8d0b1`:

- `third_party/rust-patches/axsched` became
  `crates/thekernel-axsched`;
- `third_party/rust-patches/axpoll` became
  `crates/thekernel-axpoll`.
- the active `axtask` source selected by that checkout became
  `crates/thekernel-axtask`.

The copied crate paths were clean at that commit. The unrelated TheKernel root
`README.md` working-tree modification was not copied or modified.

## Immutable registry baselines

| New package | Upstream registry baseline | Registry archive SHA-256 | Recorded source commit |
| --- | --- | --- | --- |
| `thekernel-axsched` | `axsched` `0.3.1` | `cad6b7b0b8d9ad1d52a834d8b7721114413da8cf3430af928b1c8651f911287a` | `4d86c55dce4c87dde52792515ce188081323ac07` |
| `thekernel-axpoll` | `axpoll` `0.1.2` | `36b92f85c6903350f5146216ccb7d7a7e7b4dbd6f5927a1279db03ba52a53ae7` | `86f20f6bc1b470fc21894721e72b721f49aa20b7` |
| `thekernel-axtask` | `axtask` `0.3.0-preview.2` | `bc45120776afddf28b19bb7aba87e379c5779cf28a8f7884943a4821caeec774` | `6c6765c05df0550e31edb0ca82d468199f108b3f` |

Each crate contains:

- `VENDOR.md`, which records its exact archive and upstream inventory;
- the archived upstream `Cargo.toml.orig` and `.cargo_vcs_info.json`;
- `PATCHES.md`, which records the maintained fork delta.

The preserved archive metadata remains tracked in Git but is excluded as source
input from published packages. Cargo itself conventionally generates fresh
package-time files named `Cargo.toml.orig` and `.cargo_vcs_info.json`; if those
names appear in a `.crate` archive, they describe this repository's release and
are not the byte-for-byte archived upstream files. `scripts/package-unpack.sh`
checks that the preserved upstream files did not leak into the artifact.

## License texts

All three upstream manifests declare:

`GPL-3.0-or-later OR Apache-2.0 OR MulanPSL-2.0`

The published upstream crate archives omitted license text. This distribution
adds canonical texts from the ArceOS repository at commit
`06f5953e6c2df6a316959972b6b78db78a0db5b6`, without changing the declared
license expression:

| Original ArceOS file | Distributed file | SHA-256 |
| --- | --- | --- |
| `LICENSE.Apache2` | `LICENSES/Apache-2.0.txt` | `c71d239df91726fc519c6eb72d318ec65820627232b2f796219e87dcf35d0ab4` |
| `LICENSE.GPLv3` | `LICENSES/GPL-3.0-or-later.txt` | `3972dc9744f6499f0f9b2dbf76696f2ae7ad8af9b23dde66d6af86c9dfb36986` |
| `LICENSE.MulanPSL2` | `LICENSES/MulanPSL-2.0.txt` | `3c1e0660f782af0b4b31eac50fc1f4b3c890c0d04963fe4c9d94ccf90ca52c84` |

The same three files are included under each crate so a registry artifact is
self-contained.

## Verification rule

Treat the registry archive checksum as the immutable baseline. A Git commit is
supporting context, not a substitute for a potentially dirty published source
tree. Rebase work must begin from the verified archive, preserve its authors and
license expression, and reapply every item in `PATCHES.md` with tests.
